//! Phase 5 — wiring the generic limits connector into the payments flow.
//!
//! This module owns the three pieces that join the Phase 4 framework
//! ([`super::decision`]) to the live payment flow:
//!
//! - [`record_block`] — R12: persist a blocked charge as a failed attempt
//!   (`AttemptStatus::Failure` + decline-reason) and flip the intent to
//!   `IntentStatus::Failed`, so the block surfaces in analytics (KafkaStore).
//! - [`evaluate_and_record`] — the single helper invoked at both insertion
//!   points: builds the provider request from `payment_data`, runs the
//!   decision, records the block on a `Block` outcome (then returns the
//!   structured [`ApiErrorResponse::AgenticLimitExceeded`]), and stashes the
//!   reservation handle on an `Allow` outcome for settle.
//! - [`settle_on_terminal`] — best-effort settle outbox: on a terminal attempt
//!   status, asks the provider to commit/void/refund the reservation.
//!
//! All entry points are gated `#[cfg(feature = "limits")]` by the caller.

use api_models::limits::{
    LimitDeclineReason, LimitSettleOutcome, LimitsEvaluateRequest, LimitsSettleRequest,
};
use common_utils::types::MinorUnit;
use error_stack::report;
use hyperswitch_domain_models::errors::api_error_response::ApiErrorResponse;
use router_env::logger;

use super::{
    config::LimitsProfileConfig,
    decision::{LimitsDecisionService, LimitsEvaluateContext, LimitsMetadata, LimitsOutcome},
    metrics, MypeachLimitsProvider,
};
use crate::{
    core::payments::OperationSessionGetters,
    errors::RouterResult,
    routes::SessionState,
    types::{domain, storage, storage::enums as storage_enums},
};

/// A blocked charge, carrying everything `record_block` needs to persist the
/// failed attempt + the structured API error.
#[derive(Debug, Clone)]
pub struct LimitBreach {
    /// Machine-readable decline reason.
    pub reason: LimitDeclineReason,
    /// Human-readable detail for the recorded `error_reason` / logs.
    pub details: String,
    /// Limit scope reported by the provider (defaults applied when absent).
    pub scope: String,
    /// Limit period reported by the provider.
    pub period: String,
    /// Currency of the reported amounts.
    pub currency: String,
    /// Amount already used within the window, in minor units.
    pub used_minor: i64,
    /// Configured limit for the window, in minor units.
    pub max_minor: i64,
    /// The charge amount attempted, in minor units.
    pub attempted_minor: i64,
}

impl LimitBreach {
    /// Build the structured [`ApiErrorResponse::AgenticLimitExceeded`] returned
    /// to the caller after the block is recorded.
    fn to_api_error(&self) -> ApiErrorResponse {
        ApiErrorResponse::AgenticLimitExceeded {
            scope: self.scope.clone(),
            period: self.period.clone(),
            currency: self.currency.clone(),
            used_minor: self.used_minor,
            max_minor: self.max_minor,
            attempted_minor: self.attempted_minor,
            reason: decline_reason_label(self.reason).to_string(),
        }
    }
}

/// R12: persist a limits-blocked charge as a **failed attempt** + flip the
/// intent to `Failed`.
///
/// Best-effort: both writes log-and-continue on failure (the caller still
/// returns the structured error). The attempt update mirrors the connector
/// error path in `operations/payment_response.rs` (the v1 `ErrorUpdate`
/// variant) so the failure looks identical to a connector decline downstream
/// (KafkaStore -> analytics). The intent update mirrors the FRM-cancel path in
/// `operations/payment_confirm.rs`.
pub async fn record_block<F, D>(
    state: &SessionState,
    payment_data: &D,
    processor: &domain::Processor,
    breach: &LimitBreach,
) where
    F: Clone,
    D: OperationSessionGetters<F>,
{
    let storage_scheme = processor.get_account().storage_scheme;
    let key_store = processor.get_key_store();
    let attempt = payment_data.get_payment_attempt().clone();
    let intent = payment_data.get_payment_intent().clone();

    let error_message = format!("Agentic spending limit exceeded: {}", breach.details);

    // (1) Record the attempt as a failed charge with the decline reason.
    let attempt_update = storage::PaymentAttemptUpdate::ErrorUpdate {
        connector: None,
        status: storage_enums::AttemptStatus::Failure,
        error_code: Some(Some("AGENTIC_LIMIT_EXCEEDED".to_string())),
        error_message: Some(Some(error_message.clone())),
        error_reason: Some(Some(breach.details.clone())),
        amount_capturable: Some(MinorUnit::new(0)),
        updated_by: storage_scheme.to_string(),
        unified_code: None,
        unified_message: None,
        standardised_code: None,
        description: None,
        user_guidance_message: None,
        connector_transaction_id: None,
        connector_response_reference_id: None,
        payment_method_data: None,
        encrypted_payment_method_data: None,
        authentication_type: None,
        issuer_error_code: None,
        issuer_error_message: None,
        network_details: None,
        network_error_message: None,
        advice_message: None,
        recommended_action: None,
        card_network: None,
    };

    if let Err(err) = state
        .store
        .update_payment_attempt_with_attempt_id(attempt, attempt_update, storage_scheme, key_store)
        .await
    {
        logger::error!(error = ?err, "limits: failed to record blocked attempt as Failure");
    }

    // (2) Flip the intent to Failed so the payment is terminal.
    let intent_update = storage::PaymentIntentUpdate::PGStatusUpdate {
        status: storage_enums::IntentStatus::Failed,
        incremental_authorization_allowed: None,
        updated_by: storage_scheme.to_string(),
        feature_metadata: intent
            .feature_metadata
            .clone()
            .map(hyperswitch_masking::Secret::new),
    };

    if let Err(err) = state
        .store
        .update_payment_intent(intent, intent_update, key_store, storage_scheme)
        .await
    {
        logger::error!(error = ?err, "limits: failed to flip intent to Failed after block");
    }
}

/// The single helper invoked at both payment-flow insertion points.
///
/// Builds the provider request from `payment_data`, runs the Phase 4 decision
/// pipeline, and:
/// - on `Block` -> records the failed attempt ([`record_block`]) then returns
///   [`ApiErrorResponse::AgenticLimitExceeded`];
/// - on `Allow` -> stashes the reservation handle (for settle) and returns
///   `Ok(())`.
///
/// `processor` provides the processor merchant id (NOT the platform/provider
/// merchant id), the key store, and the storage scheme used for recording.
pub async fn evaluate_and_record<F, D>(
    state: &SessionState,
    payment_data: &mut D,
    processor: &domain::Processor,
    _business_profile: &domain::Profile,
) -> RouterResult<()>
where
    F: Clone,
    D: OperationSessionGetters<F>,
{
    // Global gate. When limits are disabled, never touch the provider.
    if !state.conf.limits.enabled {
        return Ok(());
    }

    let config = profile_config(state);

    // Trigger metadata: agentic charges are flagged via `metadata.agentic`.
    let metadata = LimitsMetadata {
        agentic: is_agentic(payment_data),
    };

    let request = match build_request(payment_data, processor, metadata.agentic) {
        Some(request) => request,
        None => {
            // Without a payment-method id we cannot evaluate; fall open (the
            // provider keys reservations on the pm id).
            logger::debug!("limits: no payment_method_id on attempt, skipping evaluation");
            return Ok(());
        }
    };

    let provider = match MypeachLimitsProvider::new(
        config.provider_base_url.clone(),
        config.hmac_secret.clone(),
        std::time::Duration::from_millis(config.timeout_ms),
    ) {
        Ok(provider) => provider,
        Err(err) => {
            logger::error!(error = ?err, "limits: failed to build provider");
            // Provider construction failure is treated as unavailability: honor
            // the configured fail-mode rather than silently allowing.
            if config.fail_closed {
                let breach = unavailable_breach(&request, "provider_construction_failed");
                record_block(state, payment_data, processor, &breach).await;
                return Err(report!(breach.to_api_error()));
            }
            return Ok(());
        }
    };

    let service =
        LimitsDecisionService::new(config.circuit_breaker_threshold, BREAKER_COOLDOWN_SECS);
    let ctx = LimitsEvaluateContext::from_request(request.clone());

    let outcome = service
        .decide(&provider, &config, state.conf.limits.enabled, &metadata, &ctx)
        .await;

    match outcome {
        LimitsOutcome::Allow { reservation_id } => {
            if let Some(reservation_id) = reservation_id {
                stash_reservation(&request.tx_id, reservation_id);
            }
            Ok(())
        }
        LimitsOutcome::Block { reason, details } => {
            let breach = block_breach(&request, reason, details);
            record_block(state, payment_data, processor, &breach).await;
            Err(report!(breach.to_api_error()))
        }
    }
}

/// Best-effort settle outbox (CC1).
///
/// On a terminal attempt status, asks the provider to commit/void/refund the
/// reservation stashed at evaluate time. Idempotent at the provider (keyed on
/// `tx_id`); a no-op when no reservation was recorded for the payment.
///
/// TODO(Phase 5 / CC1 — durable outbox): this is a best-effort, in-process,
/// fire-after-await settle. It does NOT survive a crash between the terminal
/// transition and the provider call, and the reservation handle lives only in
/// the in-process [`RESERVATIONS`] map (lost on restart / not shared across
/// nodes). The durable design is the process-tracker outbox (model
/// `workflows/payment_sync.rs` + a new `ProcessTrackerRunner` variant +
/// scheduler registration), at-least-once with idempotency on
/// `payment_id`/`reservation_id`. That integration requires a new
/// `diesel_models` enum variant + a full workflow impl and is deferred.
pub async fn settle_on_terminal(
    state: &SessionState,
    tx_id: String,
    status: storage_enums::AttemptStatus,
    captured_amount_minor: Option<i64>,
    refunded_amount_minor: Option<i64>,
) {
    if !state.conf.limits.enabled {
        return;
    }
    if !status.is_terminal_status() {
        return;
    }

    let reservation_id = match take_reservation(&tx_id) {
        Some(reservation_id) => reservation_id,
        None => {
            // No reservation recorded for this payment (non-agentic, fast-path
            // skip, or already settled): nothing to do.
            return;
        }
    };

    let outcome = settle_outcome_for(status);
    let req = LimitsSettleRequest {
        tx_id: tx_id.clone(),
        reservation_id,
        outcome,
        captured_amount_minor,
        refunded_amount_minor,
    };

    let config = profile_config(state);
    let provider = match MypeachLimitsProvider::new(
        config.provider_base_url.clone(),
        config.hmac_secret.clone(),
        std::time::Duration::from_millis(config.timeout_ms),
    ) {
        Ok(provider) => provider,
        Err(err) => {
            logger::error!(error = ?err, tx_id = %tx_id, "limits: settle skipped, provider build failed");
            return;
        }
    };

    use super::LimitsProvider;
    match provider.settle(req).await {
        Ok(()) => {
            metrics::LIMITS_SETTLED.add(
                1,
                router_env::metric_attributes!(("outcome", settle_outcome_label(outcome))),
            );
            logger::debug!(tx_id = %tx_id, "limits: settle delivered");
        }
        Err(err) => {
            metrics::LIMITS_SETTLE_FAILED.add(1, &[]);
            // Best-effort: the provider's TTL-expiry sweep will eventually free
            // the reservation. Durable retry is the deferred process-tracker
            // outbox (see the TODO above).
            logger::error!(error = ?err, tx_id = %tx_id, "limits: settle failed (best-effort; provider TTL will reclaim)");
        }
    }
}

/// Default circuit-breaker cooldown (seconds). Mirrors the FRM-style resilience
/// knobs; not yet surfaced per-profile.
const BREAKER_COOLDOWN_SECS: u64 = 30;

/// Build the per-charge profile config from settings (the per-profile
/// `limits_configs` table is a later phase).
fn profile_config(state: &SessionState) -> LimitsProfileConfig {
    let limits = &state.conf.limits;
    LimitsProfileConfig {
        provider_base_url: limits.provider_base_url.clone(),
        hmac_secret: limits.hmac_secret.clone(),
        trigger_agentic_only: limits.trigger_agentic_only,
        fail_closed: limits.fail_closed,
        timeout_ms: limits.timeout_ms,
        circuit_breaker_threshold: limits.circuit_breaker_threshold,
    }
}

/// Whether this charge was initiated through an agentic flow, read from the
/// payment intent's `metadata.agentic` boolean.
fn is_agentic<F, D>(payment_data: &D) -> bool
where
    F: Clone,
    D: OperationSessionGetters<F>,
{
    payment_data
        .get_payment_intent()
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("agentic").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Build the provider `evaluate` request from `payment_data`. Returns `None`
/// when there is no payment-method id to key the reservation on.
fn build_request<F, D>(
    payment_data: &D,
    processor: &domain::Processor,
    agentic: bool,
) -> Option<LimitsEvaluateRequest>
where
    F: Clone,
    D: OperationSessionGetters<F>,
{
    let attempt = payment_data.get_payment_attempt();
    let intent = payment_data.get_payment_intent();

    let payment_method_id = attempt.payment_method_id.clone()?;

    Some(LimitsEvaluateRequest {
        tx_id: intent.payment_id.get_string_repr().to_string(),
        attempt_id: attempt.attempt_id.clone(),
        payment_method_id,
        // Processor merchant id (whose credentials execute the charge), NOT the
        // platform/provider merchant id.
        processor_merchant_id: processor
            .get_processor_merchant_id()
            .inner()
            .get_string_repr()
            .to_string(),
        amount_minor: attempt.get_total_amount().get_amount_as_i64(),
        currency: payment_data.get_currency(),
        agentic,
        intent_mandate_id: attempt.mandate_id.clone(),
    })
}

/// Map a provider `Block` decision (with its detail string) onto a [`LimitBreach`].
fn block_breach(
    request: &LimitsEvaluateRequest,
    reason: LimitDeclineReason,
    details: String,
) -> LimitBreach {
    LimitBreach {
        reason,
        details,
        scope: "unknown".to_string(),
        period: "unknown".to_string(),
        currency: request.currency.to_string(),
        used_minor: 0,
        max_minor: 0,
        attempted_minor: request.amount_minor,
    }
}

/// A breach synthesised when the provider is unavailable and the profile is
/// fail-closed.
fn unavailable_breach(request: &LimitsEvaluateRequest, cause: &str) -> LimitBreach {
    LimitBreach {
        reason: LimitDeclineReason::ProviderUnavailable,
        details: format!("limits provider unavailable ({cause}); fail-closed"),
        scope: "unknown".to_string(),
        period: "unknown".to_string(),
        currency: request.currency.to_string(),
        used_minor: 0,
        max_minor: 0,
        attempted_minor: request.amount_minor,
    }
}

/// Map a terminal attempt status onto the settle outcome reported to the
/// provider.
fn settle_outcome_for(status: storage_enums::AttemptStatus) -> LimitSettleOutcome {
    match status {
        storage_enums::AttemptStatus::Charged
        | storage_enums::AttemptStatus::PartialCharged
        | storage_enums::AttemptStatus::PartialChargedAndChargeable => LimitSettleOutcome::Succeeded,
        storage_enums::AttemptStatus::Voided
        | storage_enums::AttemptStatus::VoidedPostCharge => LimitSettleOutcome::Voided,
        storage_enums::AttemptStatus::AutoRefunded => LimitSettleOutcome::Refunded,
        // RouterDeclined / Failure / CaptureFailed / VoidFailed / Expired / ...
        _ => LimitSettleOutcome::Failed,
    }
}

fn settle_outcome_label(outcome: LimitSettleOutcome) -> &'static str {
    match outcome {
        LimitSettleOutcome::Succeeded => "succeeded",
        LimitSettleOutcome::Failed => "failed",
        LimitSettleOutcome::Voided => "voided",
        LimitSettleOutcome::Refunded => "refunded",
    }
}

fn decline_reason_label(reason: LimitDeclineReason) -> &'static str {
    match reason {
        LimitDeclineReason::LimitExceeded => "limit_exceeded",
        LimitDeclineReason::FxUnavailable => "fx_unavailable",
        LimitDeclineReason::NotEnabled => "not_enabled",
        LimitDeclineReason::ProviderUnavailable => "provider_unavailable",
    }
}

// --- in-process reservation side-map (best-effort settle) ---
//
// Reservation handles are stashed at evaluate time keyed on `tx_id`
// (payment id) and taken at settle time. This is intentionally in-process and
// non-durable; see the `settle_on_terminal` TODO for the durable design.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

fn reservations() -> &'static Mutex<HashMap<String, String>> {
    static RESERVATIONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    RESERVATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stash_reservation(tx_id: &str, reservation_id: String) {
    if let Ok(mut map) = reservations().lock() {
        map.insert(tx_id.to_string(), reservation_id);
    }
}

fn take_reservation(tx_id: &str) -> Option<String> {
    reservations().lock().ok().and_then(|mut map| map.remove(tx_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_round_trip_stash_then_take() {
        let tx_id = "pay_wiring_test_1";
        stash_reservation(tx_id, "res_xyz".to_string());
        assert_eq!(take_reservation(tx_id), Some("res_xyz".to_string()));
        // Taken once; subsequent take is empty (idempotent settle guard).
        assert_eq!(take_reservation(tx_id), None);
    }

    #[test]
    fn settle_outcome_mapping() {
        assert_eq!(
            settle_outcome_for(storage_enums::AttemptStatus::Charged),
            LimitSettleOutcome::Succeeded
        );
        assert_eq!(
            settle_outcome_for(storage_enums::AttemptStatus::Voided),
            LimitSettleOutcome::Voided
        );
        assert_eq!(
            settle_outcome_for(storage_enums::AttemptStatus::AutoRefunded),
            LimitSettleOutcome::Refunded
        );
        assert_eq!(
            settle_outcome_for(storage_enums::AttemptStatus::Failure),
            LimitSettleOutcome::Failed
        );
    }

    #[test]
    fn breach_to_api_error_carries_structured_fields() {
        let breach = LimitBreach {
            reason: LimitDeclineReason::LimitExceeded,
            details: "blocked: limit_exceeded".to_string(),
            scope: "payment_method".to_string(),
            period: "month".to_string(),
            currency: "USD".to_string(),
            used_minor: 9_500,
            max_minor: 10_000,
            attempted_minor: 1_000,
        };
        match breach.to_api_error() {
            ApiErrorResponse::AgenticLimitExceeded {
                scope,
                reason,
                attempted_minor,
                ..
            } => {
                assert_eq!(scope, "payment_method");
                assert_eq!(reason, "limit_exceeded");
                assert_eq!(attempted_minor, 1_000);
            }
            other => panic!("expected AgenticLimitExceeded, got {other:?}"),
        }
    }
}
