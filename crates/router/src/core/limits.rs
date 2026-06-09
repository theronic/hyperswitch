//! Generic spending-limits connector (Phase 4 — framework only).
//!
//! Hyperswitch consults an external limits provider per-charge, modeled on the
//! FRM connector: a two-phase lifecycle (`evaluate` -> `settle`) plus a
//! `has-limits` fast-path. This module is the **framework**: the provider
//! abstraction lives in `hyperswitch_interfaces` ([`hyperswitch_interfaces::api::limits`]);
//! here we provide
//!
//! - [`mypeach::MypeachLimitsProvider`] — the HTTP (HMAC-signed) `mypeach` impl,
//! - [`config::LimitsProfileConfig`] — per-profile configuration,
//! - [`decision::LimitsDecisionService`] — the gate + trigger + fast-path +
//!   circuit-breaker + timeout + fail-mode orchestration,
//! - [`circuit_breaker::CircuitBreaker`] — an in-memory consecutive-failures
//!   breaker.
//!
//! It is **not** wired into the payments flow — that is Phase 5. Everything
//! here is exercised by a mock provider in the unit tests.

pub mod circuit_breaker;
pub mod config;
pub mod decision;
pub mod metrics;
pub mod mypeach;
pub mod wiring;

#[cfg(test)]
mod tests;

pub use circuit_breaker::CircuitBreaker;
pub use config::LimitsProfileConfig;
pub use decision::{LimitsDecisionService, LimitsEvaluateContext, LimitsMetadata, LimitsOutcome};
pub use mypeach::MypeachLimitsProvider;
pub use wiring::{evaluate_and_record, record_block, settle_on_terminal, LimitBreach};

// Re-export the provider abstraction for ergonomic `core::limits::LimitsProvider`.
pub use hyperswitch_interfaces::api::limits::{LimitsProvider, LimitsProviderError};
