//! The limits decision service.
//!
//! Orchestrates a single charge's limits check around a [`LimitsProvider`],
//! applying the per-profile gate, the trigger rule, the `has-limits` fast-path,
//! the circuit breaker, the per-call timeout, and the configured fail-mode. It
//! is pure orchestration: no DB, no payment-flow types — Phase 5 wires it in.

use api_models::limits::{LimitDecision, LimitDeclineReason, LimitsEvaluateRequest};
use hyperswitch_interfaces::api::limits::{LimitsProvider, LimitsProviderError};
use router_env::logger;

use super::{circuit_breaker::CircuitBreaker, config::LimitsProfileConfig, metrics};

/// Charge-level metadata used to decide whether limits apply at all.
///
/// Phase 4 keeps this decoupled from the payment-flow types; Phase 5 maps the
/// real payment metadata onto it.
#[derive(Debug, Clone, Default)]
pub struct LimitsMetadata {
    /// Whether the charge was initiated through an agentic flow.
    pub agentic: bool,
}

/// Everything the decision service needs to evaluate one charge.
#[derive(Debug, Clone)]
pub struct LimitsEvaluateContext {
    /// The wire request handed to the provider's `evaluate`.
    pub request: LimitsEvaluateRequest,
    /// Customer reference used for the `has-limits` fast-path. Defaults to the
    /// request's payment-method id when not set explicitly.
    pub customer_ref: String,
}

impl LimitsEvaluateContext {
    /// Build a context from a request, using the payment-method id as the
    /// `has-limits` customer reference.
    pub fn from_request(request: LimitsEvaluateRequest) -> Self {
        let customer_ref = request.payment_method_id.clone();
        Self {
            request,
            customer_ref,
        }
    }
}

/// The terminal result of a limits check, consumed by the (Phase 5) caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitsOutcome {
    /// The charge may proceed. Carries the reservation handle when the provider
    /// returned one (absent for short-circuits / fast-path skips).
    Allow {
        /// Reservation handle to present at settle time, if any.
        reservation_id: Option<String>,
    },
    /// The charge is blocked. Carries the machine-readable decline reason and a
    /// human-readable detail string for logs/analytics.
    Block {
        /// Why the charge was blocked.
        reason: LimitDeclineReason,
        /// Human-readable detail for logging / the recorded failure (Phase 5).
        details: String,
    },
}

impl LimitsOutcome {
    /// Convenience: an allow with no reservation (short-circuit paths).
    fn allow_no_reservation() -> Self {
        Self::Allow {
            reservation_id: None,
        }
    }
}

/// Stateless-config + breaker bundle that decides one charge at a time.
///
/// The circuit breaker is owned here so its consecutive-failure state persists
/// across `decide` calls for the lifetime of the service.
pub struct LimitsDecisionService {
    breaker: CircuitBreaker,
}

impl LimitsDecisionService {
    /// Build a decision service whose breaker opens after
    /// `circuit_breaker_threshold` consecutive failures.
    pub fn new(circuit_breaker_threshold: u32, breaker_cooldown_secs: u64) -> Self {
        Self {
            breaker: CircuitBreaker::new(
                circuit_breaker_threshold,
                std::time::Duration::from_secs(breaker_cooldown_secs),
            ),
        }
    }

    /// Whether limits should be checked for a charge with this metadata.
    ///
    /// Default policy: only agentic charges trigger a check. When the profile
    /// sets `trigger_agentic_only = false`, every charge is checked.
    pub fn should_check_limits(
        &self,
        config: &LimitsProfileConfig,
        metadata: &LimitsMetadata,
    ) -> bool {
        if config.trigger_agentic_only {
            metadata.agentic
        } else {
            true
        }
    }

    /// Run the full decision pipeline for one charge.
    ///
    /// Order of operations:
    /// 1. `enabled` gate + trigger rule -> `Allow` (no provider call).
    /// 2. circuit breaker open -> fail-mode.
    /// 3. `has_limits` fast-path -> `Allow` when the subject has no limits.
    /// 4. `evaluate` -> map the decision; transport failure -> fail-mode.
    pub async fn decide<P: LimitsProvider + ?Sized>(
        &self,
        provider: &P,
        config: &LimitsProfileConfig,
        enabled: bool,
        metadata: &LimitsMetadata,
        ctx: &LimitsEvaluateContext,
    ) -> LimitsOutcome {
        // (1) gate + trigger.
        if !enabled {
            logger::debug!("limits: connector disabled, allowing charge");
            return LimitsOutcome::allow_no_reservation();
        }
        if !self.should_check_limits(config, metadata) {
            metrics::LIMITS_SKIPPED_NOT_TRIGGERED.add(1, &[]);
            logger::debug!("limits: trigger rule not met (non-agentic), allowing charge");
            return LimitsOutcome::allow_no_reservation();
        }

        // (2) circuit breaker.
        if self.breaker.is_open() {
            metrics::LIMITS_BREAKER_OPEN.add(1, &[]);
            logger::warn!("limits: circuit breaker open");
            return self.fail_mode_outcome(config, "circuit_breaker_open");
        }

        // (3) has-limits fast-path. A transport failure here is non-fatal: fall
        // through to a full evaluate rather than failing the charge.
        match provider.has_limits(&ctx.customer_ref).await {
            Ok(false) => {
                metrics::LIMITS_FASTPATH_NO_LIMITS.add(1, &[]);
                logger::debug!("limits: has-limits=false, fast-path allow");
                return LimitsOutcome::allow_no_reservation();
            }
            Ok(true) => {
                // Best-effort probe: only the `evaluate` path drives the circuit
                // breaker. Recording success here would reset the consecutive-
                // failure counter before every evaluate, so the breaker could
                // never open under sustained evaluate failures.
            }
            Err(err) => {
                logger::warn!(error = ?err, "limits: has-limits probe failed, proceeding to evaluate");
            }
        }

        // (4) evaluate.
        match provider.evaluate(ctx.request.clone()).await {
            Ok(response) => {
                self.breaker.record_success();
                self.map_evaluate_response(response)
            }
            Err(err) => {
                self.breaker.record_failure();
                self.handle_provider_error(config, err)
            }
        }
    }

    /// Map a successful `evaluate` response onto an outcome.
    fn map_evaluate_response(
        &self,
        response: api_models::limits::LimitsEvaluateResponse,
    ) -> LimitsOutcome {
        match response.decision {
            LimitDecision::Allow => {
                metrics::LIMITS_ALLOWED.add(1, &[]);
                LimitsOutcome::Allow {
                    reservation_id: response.reservation_id,
                }
            }
            LimitDecision::Block => {
                let reason = response.reason.unwrap_or(LimitDeclineReason::LimitExceeded);
                metrics::LIMITS_BLOCKED.add(
                    1,
                    router_env::metric_attributes!(("reason", decline_reason_label(reason))),
                );
                let details = format_block_details(&response, reason);
                logger::info!(reason = ?reason, "limits: charge blocked");
                LimitsOutcome::Block { reason, details }
            }
        }
    }

    /// Map a provider transport error onto the configured fail-mode.
    fn handle_provider_error(
        &self,
        config: &LimitsProfileConfig,
        err: LimitsProviderError,
    ) -> LimitsOutcome {
        metrics::LIMITS_PROVIDER_ERROR.add(
            1,
            router_env::metric_attributes!(("kind", provider_error_label(&err))),
        );
        logger::error!(error = ?err, fail_closed = config.fail_closed, "limits: provider error");
        self.fail_mode_outcome(config, provider_error_label(&err))
    }

    /// Resolve the fail-mode: fail-closed blocks; fail-open allows.
    fn fail_mode_outcome(&self, config: &LimitsProfileConfig, cause: &str) -> LimitsOutcome {
        if config.fail_closed {
            metrics::LIMITS_FAIL_CLOSED.add(1, &[]);
            LimitsOutcome::Block {
                reason: LimitDeclineReason::ProviderUnavailable,
                details: format!("limits provider unavailable ({cause}); fail-closed"),
            }
        } else {
            metrics::LIMITS_FAIL_OPEN.add(1, &[]);
            LimitsOutcome::Allow {
                reservation_id: None,
            }
        }
    }
}

/// Stable label for the decline-reason taxonomy (metrics + logs).
fn decline_reason_label(reason: LimitDeclineReason) -> &'static str {
    match reason {
        LimitDeclineReason::LimitExceeded => "limit_exceeded",
        LimitDeclineReason::FxUnavailable => "fx_unavailable",
        LimitDeclineReason::NotEnabled => "not_enabled",
        LimitDeclineReason::ProviderUnavailable => "provider_unavailable",
    }
}

/// Stable label for the provider-error taxonomy (metrics + logs).
fn provider_error_label(err: &LimitsProviderError) -> &'static str {
    match err {
        LimitsProviderError::Timeout => "timeout",
        LimitsProviderError::Http { .. } => "http",
        LimitsProviderError::Decode(_) => "decode",
        LimitsProviderError::Unavailable(_) => "unavailable",
    }
}

fn format_block_details(
    response: &api_models::limits::LimitsEvaluateResponse,
    reason: LimitDeclineReason,
) -> String {
    let mut details = format!("blocked: {}", decline_reason_label(reason));
    if let (Some(used), Some(max)) = (response.used_minor, response.max_minor) {
        details.push_str(&format!(" (used {used} of {max}"));
        if let Some(scope) = &response.scope {
            details.push_str(&format!(", scope {scope}"));
        }
        if let Some(period) = &response.period {
            details.push_str(&format!(", period {period}"));
        }
        details.push(')');
    }
    details
}
