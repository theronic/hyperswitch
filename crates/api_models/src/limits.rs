//! Wire-contract types for the generic spending-limits provider.
//!
//! Hyperswitch consults an external limits provider per-charge, modeled on the
//! FRM (fraud) connector pattern: a two-phase lifecycle of
//! `evaluate` (atomic reserve) -> `settle` (commit | void | refund), plus a
//! `has-limits` fast-path hint.
//!
//! These are **serde-only** wire types (request/response DTOs sent over HTTP);
//! they are intentionally **not** diesel storage enums and must never carry
//! cardholder data (PAN/SAD). The contract exposes typed, non-CHD fields only;
//! a test in this module asserts the serialized field set is a subset of an
//! explicit allowlist so no card-like field can ever appear on the wire.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Phase 1 request: ask the provider to atomically reserve budget for a charge.
///
/// `POST /limits/v1/evaluate`
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LimitsEvaluateRequest {
    /// Provider-opaque transaction identifier (Hyperswitch payment id).
    pub tx_id: String,
    /// Identifier of the specific payment attempt being evaluated.
    pub attempt_id: String,
    /// Identifier of the payment method the charge is drawn against.
    pub payment_method_id: String,
    /// The processor/merchant the charge belongs to.
    pub processor_merchant_id: String,
    /// Charge amount in the currency's minor units (e.g. cents).
    pub amount_minor: i64,
    /// Currency of `amount_minor`.
    #[schema(value_type = Currency)]
    pub currency: common_enums::Currency,
    /// Whether this charge was initiated through an agentic flow.
    pub agentic: bool,
    /// Optional mandate/intent identifier associated with the charge.
    pub intent_mandate_id: Option<String>,
}

/// Phase 1 response: the provider's reserve decision and (on block) the reason.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LimitsEvaluateResponse {
    /// Whether the charge is allowed or blocked.
    pub decision: LimitDecision,
    /// Reservation handle to be presented at settle time (present on `allow`).
    pub reservation_id: Option<String>,
    /// Machine-readable decline reason (present on `block`).
    pub reason: Option<LimitDeclineReason>,
    /// The limit scope that produced the decision (e.g. payment-method/merchant/global).
    pub scope: Option<String>,
    /// The limit period that produced the decision (e.g. day/week/month).
    pub period: Option<String>,
    /// Currency of the reported `used_minor`/`max_minor`/`converted_minor` values.
    #[schema(value_type = Option<Currency>)]
    pub currency: Option<common_enums::Currency>,
    /// Amount already used within the window, in minor units.
    pub used_minor: Option<i64>,
    /// Configured limit for the window, in minor units.
    pub max_minor: Option<i64>,
    /// The charge amount converted into the limit currency, in minor units.
    pub converted_minor: Option<i64>,
}

/// Phase 2 request: commit, void, or refund a prior reservation.
///
/// `POST /limits/v1/settle` — idempotent on `tx_id`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LimitsSettleRequest {
    /// Provider-opaque transaction identifier (Hyperswitch payment id).
    pub tx_id: String,
    /// Reservation handle returned by the matching `evaluate` call.
    pub reservation_id: String,
    /// Terminal outcome of the charge.
    pub outcome: LimitSettleOutcome,
    /// Amount actually captured, in minor units (present on capture).
    pub captured_amount_minor: Option<i64>,
    /// Amount refunded, in minor units (present on refund).
    pub refunded_amount_minor: Option<i64>,
}

/// Phase 2 response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LimitsSettleResponse {
    /// Whether the settle was applied.
    pub ok: bool,
}

/// `GET /limits/v1/has-limits` response: fast-path hint to skip evaluation for
/// the no-limit majority.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LimitsHasLimitsResponse {
    /// Whether any spending limit applies and evaluation is required.
    pub has_limits: bool,
}

/// The reserve decision returned by `evaluate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitDecision {
    /// The charge is within limits and budget was reserved.
    Allow,
    /// The charge is blocked.
    Block,
}

/// Machine-readable reason a charge was blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitDeclineReason {
    /// The charge would exceed a configured limit.
    LimitExceeded,
    /// Cross-currency conversion was required but FX rates were unavailable.
    FxUnavailable,
    /// Limits are not enabled for the subject.
    NotEnabled,
    /// The limits provider could not be reached or errored.
    ProviderUnavailable,
}

/// Terminal outcome reported to `settle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitSettleOutcome {
    /// The charge succeeded; commit the reservation.
    Succeeded,
    /// The charge failed; void the reservation.
    Failed,
    /// The charge was voided; void the reservation.
    Voided,
    /// The charge was refunded; restore the budget.
    Refunded,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{json, Value};

    use super::*;

    fn sample_evaluate_request() -> LimitsEvaluateRequest {
        LimitsEvaluateRequest {
            tx_id: "pay_123".to_string(),
            attempt_id: "att_123".to_string(),
            payment_method_id: "pm_123".to_string(),
            processor_merchant_id: "merch_123".to_string(),
            amount_minor: 1000,
            currency: common_enums::Currency::USD,
            agentic: true,
            intent_mandate_id: Some("mandate_123".to_string()),
        }
    }

    fn sample_settle_request() -> LimitsSettleRequest {
        LimitsSettleRequest {
            tx_id: "pay_123".to_string(),
            reservation_id: "res_123".to_string(),
            outcome: LimitSettleOutcome::Refunded,
            captured_amount_minor: Some(800),
            refunded_amount_minor: Some(200),
        }
    }

    // --- serde round-trip: structs ---

    #[test]
    fn evaluate_request_round_trip() {
        let original = sample_evaluate_request();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: LimitsEvaluateRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.tx_id, original.tx_id);
        assert_eq!(back.attempt_id, original.attempt_id);
        assert_eq!(back.payment_method_id, original.payment_method_id);
        assert_eq!(back.processor_merchant_id, original.processor_merchant_id);
        assert_eq!(back.amount_minor, original.amount_minor);
        assert_eq!(back.currency, original.currency);
        assert_eq!(back.agentic, original.agentic);
        assert_eq!(back.intent_mandate_id, original.intent_mandate_id);
    }

    #[test]
    fn evaluate_request_wire_shape() {
        let value = serde_json::to_value(sample_evaluate_request()).expect("serialize");
        assert_eq!(
            value,
            json!({
                "tx_id": "pay_123",
                "attempt_id": "att_123",
                "payment_method_id": "pm_123",
                "processor_merchant_id": "merch_123",
                "amount_minor": 1000,
                "currency": "USD",
                "agentic": true,
                "intent_mandate_id": "mandate_123",
            })
        );
    }

    #[test]
    fn evaluate_response_round_trip() {
        let original = LimitsEvaluateResponse {
            decision: LimitDecision::Block,
            reservation_id: None,
            reason: Some(LimitDeclineReason::LimitExceeded),
            scope: Some("payment_method".to_string()),
            period: Some("month".to_string()),
            currency: Some(common_enums::Currency::EUR),
            used_minor: Some(9000),
            max_minor: Some(10000),
            converted_minor: Some(1500),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: LimitsEvaluateResponse = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.decision, original.decision);
        assert_eq!(back.reservation_id, original.reservation_id);
        assert_eq!(back.reason, original.reason);
        assert_eq!(back.scope, original.scope);
        assert_eq!(back.period, original.period);
        assert_eq!(back.currency, original.currency);
        assert_eq!(back.used_minor, original.used_minor);
        assert_eq!(back.max_minor, original.max_minor);
        assert_eq!(back.converted_minor, original.converted_minor);
    }

    #[test]
    fn evaluate_response_allow_wire_shape() {
        let value = serde_json::to_value(LimitsEvaluateResponse {
            decision: LimitDecision::Allow,
            reservation_id: Some("res_123".to_string()),
            reason: None,
            scope: None,
            period: None,
            currency: None,
            used_minor: None,
            max_minor: None,
            converted_minor: None,
        })
        .expect("serialize");

        assert_eq!(value["decision"], json!("allow"));
        assert_eq!(value["reservation_id"], json!("res_123"));
        assert_eq!(value["reason"], Value::Null);
    }

    #[test]
    fn settle_request_round_trip() {
        let original = sample_settle_request();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: LimitsSettleRequest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.tx_id, original.tx_id);
        assert_eq!(back.reservation_id, original.reservation_id);
        assert_eq!(back.outcome, original.outcome);
        assert_eq!(back.captured_amount_minor, original.captured_amount_minor);
        assert_eq!(back.refunded_amount_minor, original.refunded_amount_minor);
    }

    #[test]
    fn settle_request_wire_shape() {
        let value = serde_json::to_value(sample_settle_request()).expect("serialize");
        assert_eq!(
            value,
            json!({
                "tx_id": "pay_123",
                "reservation_id": "res_123",
                "outcome": "refunded",
                "captured_amount_minor": 800,
                "refunded_amount_minor": 200,
            })
        );
    }

    #[test]
    fn settle_response_round_trip() {
        let original = LimitsSettleResponse { ok: true };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: LimitsSettleResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.ok, original.ok);
        assert_eq!(json, r#"{"ok":true}"#);
    }

    #[test]
    fn has_limits_response_round_trip() {
        let original = LimitsHasLimitsResponse { has_limits: true };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: LimitsHasLimitsResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.has_limits, original.has_limits);
        assert_eq!(json, r#"{"has_limits":true}"#);
    }

    // --- serde round-trip + snake_case wire format: enums ---

    #[test]
    fn limit_decision_snake_case_round_trip() {
        for (variant, wire) in [(LimitDecision::Allow, "allow"), (LimitDecision::Block, "block")] {
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: LimitDecision = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn limit_decline_reason_snake_case_round_trip() {
        for (variant, wire) in [
            (LimitDeclineReason::LimitExceeded, "limit_exceeded"),
            (LimitDeclineReason::FxUnavailable, "fx_unavailable"),
            (LimitDeclineReason::NotEnabled, "not_enabled"),
            (LimitDeclineReason::ProviderUnavailable, "provider_unavailable"),
        ] {
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: LimitDeclineReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn limit_settle_outcome_snake_case_round_trip() {
        for (variant, wire) in [
            (LimitSettleOutcome::Succeeded, "succeeded"),
            (LimitSettleOutcome::Failed, "failed"),
            (LimitSettleOutcome::Voided, "voided"),
            (LimitSettleOutcome::Refunded, "refunded"),
        ] {
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: LimitSettleOutcome = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, variant);
        }
    }

    // --- CHD allowlist: no card/PAN-like field can ever be on the wire ---

    /// Recursively collect every object key in a JSON value.
    fn collect_keys(value: &Value, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    out.insert(key.clone());
                    collect_keys(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_keys(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn request_field_set_subset_of_chd_allowlist() {
        let allowlist: BTreeSet<String> = [
            "tx_id",
            "attempt_id",
            "payment_method_id",
            "processor_merchant_id",
            "amount_minor",
            "currency",
            "agentic",
            "intent_mandate_id",
            "reservation_id",
            "outcome",
            "captured_amount_minor",
            "refunded_amount_minor",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let mut keys = BTreeSet::new();
        collect_keys(
            &serde_json::to_value(sample_evaluate_request()).expect("serialize"),
            &mut keys,
        );
        collect_keys(
            &serde_json::to_value(sample_settle_request()).expect("serialize"),
            &mut keys,
        );

        let leaked: Vec<&String> = keys.difference(&allowlist).collect();
        assert!(
            leaked.is_empty(),
            "request contract leaked non-allowlisted fields: {leaked:?}"
        );
    }
}
