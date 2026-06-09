//! Pure unit tests for the limits framework, driven by a mock provider.
//!
//! No DB, no payment flow: these exercise [`LimitsDecisionService`] and the
//! circuit breaker against a configurable [`MockLimitsProvider`].

use std::sync::atomic::{AtomicUsize, Ordering};

use api_models::limits::{
    LimitDecision, LimitDeclineReason, LimitsEvaluateRequest, LimitsEvaluateResponse,
    LimitsSettleRequest,
};
use async_trait::async_trait;

use super::{
    config::LimitsProfileConfig,
    decision::{LimitsDecisionService, LimitsEvaluateContext, LimitsMetadata, LimitsOutcome},
    LimitsProvider, LimitsProviderError,
};

/// How the mock should respond to `evaluate`.
#[derive(Clone)]
enum EvaluateBehavior {
    Allow(Option<String>),
    Block(LimitDeclineReason),
    Err(LimitsProviderError),
}

/// A fully controllable [`LimitsProvider`] that counts its calls.
struct MockLimitsProvider {
    has_limits: Result<bool, LimitsProviderError>,
    evaluate: EvaluateBehavior,
    evaluate_calls: AtomicUsize,
    has_limits_calls: AtomicUsize,
    settle_calls: AtomicUsize,
}

impl MockLimitsProvider {
    fn allow() -> Self {
        Self {
            has_limits: Ok(true),
            evaluate: EvaluateBehavior::Allow(Some("res_1".to_string())),
            evaluate_calls: AtomicUsize::new(0),
            has_limits_calls: AtomicUsize::new(0),
            settle_calls: AtomicUsize::new(0),
        }
    }

    fn with_evaluate(mut self, behavior: EvaluateBehavior) -> Self {
        self.evaluate = behavior;
        self
    }

    fn with_has_limits(mut self, value: Result<bool, LimitsProviderError>) -> Self {
        self.has_limits = value;
        self
    }

    fn evaluate_calls(&self) -> usize {
        self.evaluate_calls.load(Ordering::SeqCst)
    }

    fn has_limits_calls(&self) -> usize {
        self.has_limits_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LimitsProvider for MockLimitsProvider {
    async fn evaluate(
        &self,
        _req: LimitsEvaluateRequest,
    ) -> Result<LimitsEvaluateResponse, LimitsProviderError> {
        self.evaluate_calls.fetch_add(1, Ordering::SeqCst);
        match &self.evaluate {
            EvaluateBehavior::Allow(reservation_id) => Ok(LimitsEvaluateResponse {
                decision: LimitDecision::Allow,
                reservation_id: reservation_id.clone(),
                reason: None,
                scope: None,
                period: None,
                currency: None,
                used_minor: None,
                max_minor: None,
                converted_minor: None,
            }),
            EvaluateBehavior::Block(reason) => Ok(LimitsEvaluateResponse {
                decision: LimitDecision::Block,
                reservation_id: None,
                reason: Some(*reason),
                scope: Some("payment_method".to_string()),
                period: Some("month".to_string()),
                currency: Some(common_enums::Currency::USD),
                used_minor: Some(9_500),
                max_minor: Some(10_000),
                converted_minor: Some(1_000),
            }),
            EvaluateBehavior::Err(err) => Err(err.clone()),
        }
    }

    async fn settle(&self, _req: LimitsSettleRequest) -> Result<(), LimitsProviderError> {
        self.settle_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn has_limits(&self, _customer_ref: &str) -> Result<bool, LimitsProviderError> {
        self.has_limits_calls.fetch_add(1, Ordering::SeqCst);
        self.has_limits.clone()
    }
}

// --- helpers ---

fn sample_request() -> LimitsEvaluateRequest {
    LimitsEvaluateRequest {
        tx_id: "pay_1".to_string(),
        attempt_id: "att_1".to_string(),
        payment_method_id: "pm_1".to_string(),
        processor_merchant_id: "merch_1".to_string(),
        amount_minor: 1_000,
        currency: common_enums::Currency::USD,
        agentic: true,
        intent_mandate_id: None,
    }
}

fn agentic_meta() -> LimitsMetadata {
    LimitsMetadata { agentic: true }
}

fn fail_closed_config() -> LimitsProfileConfig {
    LimitsProfileConfig {
        fail_closed: true,
        ..Default::default()
    }
}

fn fail_open_config() -> LimitsProfileConfig {
    LimitsProfileConfig {
        fail_closed: false,
        ..Default::default()
    }
}

fn service() -> LimitsDecisionService {
    LimitsDecisionService::new(3, 30)
}

async fn decide(
    provider: &MockLimitsProvider,
    config: &LimitsProfileConfig,
    enabled: bool,
    meta: &LimitsMetadata,
) -> LimitsOutcome {
    service()
        .decide(
            provider,
            config,
            enabled,
            meta,
            &LimitsEvaluateContext::from_request(sample_request()),
        )
        .await
}

// --- tests ---

#[tokio::test]
async fn allow_passes_with_reservation() {
    let provider = MockLimitsProvider::allow();
    let outcome = decide(&provider, &fail_closed_config(), true, &agentic_meta()).await;
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: Some("res_1".to_string())
        }
    );
    assert_eq!(provider.evaluate_calls(), 1);
}

#[tokio::test]
async fn block_returns_decline_reason() {
    let provider = MockLimitsProvider::allow()
        .with_evaluate(EvaluateBehavior::Block(LimitDeclineReason::LimitExceeded));
    let outcome = decide(&provider, &fail_closed_config(), true, &agentic_meta()).await;
    match outcome {
        LimitsOutcome::Block { reason, details } => {
            assert_eq!(reason, LimitDeclineReason::LimitExceeded);
            assert!(details.contains("limit_exceeded"), "details: {details}");
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_fail_closed_blocks_provider_unavailable() {
    let provider = MockLimitsProvider::allow()
        .with_evaluate(EvaluateBehavior::Err(LimitsProviderError::Timeout));
    let outcome = decide(&provider, &fail_closed_config(), true, &agentic_meta()).await;
    match outcome {
        LimitsOutcome::Block { reason, .. } => {
            assert_eq!(reason, LimitDeclineReason::ProviderUnavailable);
        }
        other => panic!("expected Block(provider_unavailable), got {other:?}"),
    }
    assert_eq!(provider.evaluate_calls(), 1);
}

#[tokio::test]
async fn timeout_fail_open_allows() {
    let provider = MockLimitsProvider::allow()
        .with_evaluate(EvaluateBehavior::Err(LimitsProviderError::Timeout));
    let outcome = decide(&provider, &fail_open_config(), true, &agentic_meta()).await;
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: None
        }
    );
}

#[tokio::test]
async fn unavailable_fail_closed_blocks() {
    let provider = MockLimitsProvider::allow().with_evaluate(EvaluateBehavior::Err(
        LimitsProviderError::Unavailable("conn refused".to_string()),
    ));
    let outcome = decide(&provider, &fail_closed_config(), true, &agentic_meta()).await;
    assert!(matches!(
        outcome,
        LimitsOutcome::Block {
            reason: LimitDeclineReason::ProviderUnavailable,
            ..
        }
    ));
}

#[tokio::test]
async fn has_limits_false_short_circuits_without_evaluate() {
    let provider = MockLimitsProvider::allow().with_has_limits(Ok(false));
    let outcome = decide(&provider, &fail_closed_config(), true, &agentic_meta()).await;
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: None
        }
    );
    assert_eq!(provider.has_limits_calls(), 1);
    assert_eq!(
        provider.evaluate_calls(),
        0,
        "evaluate must be skipped when has_limits=false"
    );
}

#[tokio::test]
async fn non_agentic_trigger_allows_without_calling_provider() {
    let provider = MockLimitsProvider::allow();
    let non_agentic = LimitsMetadata { agentic: false };
    // Default config: trigger_agentic_only = true.
    let outcome = decide(&provider, &fail_closed_config(), true, &non_agentic).await;
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: None
        }
    );
    assert_eq!(provider.has_limits_calls(), 0);
    assert_eq!(provider.evaluate_calls(), 0);
}

#[tokio::test]
async fn disabled_allows_without_calling_provider() {
    let provider = MockLimitsProvider::allow();
    let outcome = decide(&provider, &fail_closed_config(), false, &agentic_meta()).await;
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: None
        }
    );
    assert_eq!(provider.evaluate_calls(), 0);
}

#[tokio::test]
async fn trigger_all_charges_when_not_agentic_only() {
    let provider = MockLimitsProvider::allow();
    let config = LimitsProfileConfig {
        trigger_agentic_only: false,
        ..Default::default()
    };
    let non_agentic = LimitsMetadata { agentic: false };
    let outcome = decide(&provider, &config, true, &non_agentic).await;
    // trigger_agentic_only=false -> even a non-agentic charge is evaluated.
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: Some("res_1".to_string())
        }
    );
    assert_eq!(provider.evaluate_calls(), 1);
}

#[tokio::test]
async fn circuit_breaker_opens_after_threshold_failures() {
    // Threshold = 3: after 3 consecutive provider errors the breaker opens and
    // the 4th call short-circuits to the fail-mode WITHOUT calling evaluate.
    let provider = MockLimitsProvider::allow()
        .with_evaluate(EvaluateBehavior::Err(LimitsProviderError::Unavailable(
            "down".to_string(),
        )))
        // breaker-open path skips has_limits too; keep it benign.
        .with_has_limits(Ok(true));
    let svc = LimitsDecisionService::new(3, 30);
    let ctx = LimitsEvaluateContext::from_request(sample_request());
    let config = fail_closed_config();
    let meta = agentic_meta();

    for _ in 0..3 {
        let outcome = svc.decide(&provider, &config, true, &meta, &ctx).await;
        assert!(matches!(outcome, LimitsOutcome::Block { .. }));
    }
    let evaluate_calls_before = provider.evaluate_calls();
    assert_eq!(evaluate_calls_before, 3);

    // Breaker now open: this decision must not reach the provider.
    let outcome = svc.decide(&provider, &config, true, &meta, &ctx).await;
    assert!(matches!(
        outcome,
        LimitsOutcome::Block {
            reason: LimitDeclineReason::ProviderUnavailable,
            ..
        }
    ));
    assert_eq!(
        provider.evaluate_calls(),
        evaluate_calls_before,
        "evaluate must not be called while the breaker is open"
    );
}

#[tokio::test]
async fn circuit_breaker_open_fail_open_allows_without_provider_call() {
    let provider = MockLimitsProvider::allow().with_evaluate(EvaluateBehavior::Err(
        LimitsProviderError::Unavailable("down".to_string()),
    ));
    let svc = LimitsDecisionService::new(1, 30);
    let ctx = LimitsEvaluateContext::from_request(sample_request());
    let meta = agentic_meta();

    // First failure (threshold=1) opens the breaker, fail-open -> still allow.
    let first = svc.decide(&provider, &fail_open_config(), true, &meta, &ctx).await;
    assert_eq!(first, LimitsOutcome::Allow { reservation_id: None });

    // Now breaker open + fail-open -> allow without touching the provider.
    let calls_before = provider.evaluate_calls();
    let second = svc.decide(&provider, &fail_open_config(), true, &meta, &ctx).await;
    assert_eq!(second, LimitsOutcome::Allow { reservation_id: None });
    assert_eq!(provider.evaluate_calls(), calls_before);
}

#[tokio::test]
async fn has_limits_error_falls_through_to_evaluate() {
    // A failing has-limits probe is non-fatal: we still evaluate.
    let provider = MockLimitsProvider::allow()
        .with_has_limits(Err(LimitsProviderError::Unavailable("probe down".to_string())));
    let outcome = decide(&provider, &fail_closed_config(), true, &agentic_meta()).await;
    assert_eq!(
        outcome,
        LimitsOutcome::Allow {
            reservation_id: Some("res_1".to_string())
        }
    );
    assert_eq!(provider.has_limits_calls(), 1);
    assert_eq!(provider.evaluate_calls(), 1);
}
