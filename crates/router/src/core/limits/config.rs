//! Per-profile configuration for the generic limits connector.
//!
//! This is intentionally a standalone struct for Phase 4 (pragmatic; not yet
//! wired into the business-profile table). It mirrors the shape `frm_configs`
//! plays for the FRM connector: provider endpoint, trigger rule, fail-mode, and
//! resilience knobs.

use hyperswitch_masking::Secret;

/// Default per-call timeout in milliseconds when none is configured.
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;

/// Default consecutive-failure threshold before the circuit breaker opens.
pub const DEFAULT_CIRCUIT_BREAKER_THRESHOLD: u32 = 5;

/// Per-profile limits connector configuration.
#[derive(Debug, Clone)]
pub struct LimitsProfileConfig {
    /// Base URL of the limits provider (e.g. `https://mypeach.example`).
    /// The connector appends `/limits/v1/...` paths to this.
    pub provider_base_url: String,
    /// HMAC secret used to sign outbound requests.
    pub hmac_secret: Secret<String>,
    /// When `true`, only charges flagged `agentic` trigger a limits check.
    /// When `false`, every charge on this profile is checked.
    pub trigger_agentic_only: bool,
    /// Fail-mode on provider unavailability / timeout / open breaker:
    /// `true` blocks the charge (fail-closed), `false` allows it (fail-open).
    pub fail_closed: bool,
    /// Per-call timeout in milliseconds.
    pub timeout_ms: u64,
    /// Consecutive failures before the circuit breaker opens.
    pub circuit_breaker_threshold: u32,
}

impl Default for LimitsProfileConfig {
    fn default() -> Self {
        Self {
            provider_base_url: String::new(),
            hmac_secret: Secret::new(String::new()),
            trigger_agentic_only: true,
            fail_closed: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            circuit_breaker_threshold: DEFAULT_CIRCUIT_BREAKER_THRESHOLD,
        }
    }
}
