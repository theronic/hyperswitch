//! Generic spending-limits provider interface.
//!
//! Modeled on the FRM (fraud-check) connector pattern: Hyperswitch consults an
//! external policy provider per-charge over a two-phase lifecycle —
//! `evaluate` (atomic reserve) -> `settle` (commit | void | refund) — plus a
//! `has_limits` fast-path hint to skip the call for the no-limit majority.
//!
//! This is the abstract boundary only (parallel to `fraud_check_v2`). Concrete
//! transports (e.g. the HTTP `mypeach` provider) live in the `router` crate.
//! The wire DTOs are the `api_models::limits` contract types.

use api_models::limits::{LimitsEvaluateRequest, LimitsEvaluateResponse, LimitsSettleRequest};
use async_trait::async_trait;

/// Why a call to the limits provider failed.
///
/// These are *transport/availability* failures, distinct from a successful
/// `block` decision (which is carried in [`LimitsEvaluateResponse`]). The
/// decision service maps these onto the configured fail-mode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitsProviderError {
    /// The request exceeded the configured timeout.
    #[error("limits provider timed out")]
    Timeout,
    /// The provider returned a non-success HTTP status.
    #[error("limits provider returned HTTP error: status {status}")]
    Http {
        /// The HTTP status code returned by the provider.
        status: u16,
    },
    /// The provider's response body could not be decoded into the contract type.
    #[error("failed to decode limits provider response: {0}")]
    Decode(String),
    /// The provider could not be reached (connection error, DNS, TLS, etc.).
    #[error("limits provider unavailable: {0}")]
    Unavailable(String),
}

/// An external spending-limits policy provider.
///
/// Implementations are the connector boundary: they own only transport +
/// (de)serialization + signing. Policy, the ledger, and identity live behind
/// the provider (in MyPeach for the default `mypeach` impl).
#[async_trait]
pub trait LimitsProvider: Send + Sync {
    /// Phase 1: ask the provider to atomically reserve budget for a charge.
    ///
    /// A successful call returns a [`LimitsEvaluateResponse`] carrying the
    /// `allow`/`block` decision; transport failures surface as
    /// [`LimitsProviderError`].
    async fn evaluate(
        &self,
        req: LimitsEvaluateRequest,
    ) -> Result<LimitsEvaluateResponse, LimitsProviderError>;

    /// Phase 2: commit, void, or refund a prior reservation. Idempotent on
    /// `tx_id` at the provider.
    async fn settle(&self, req: LimitsSettleRequest) -> Result<(), LimitsProviderError>;

    /// Fast-path hint: whether any limit applies to `customer_ref`. When
    /// `false`, the decision service can skip [`evaluate`](Self::evaluate)
    /// entirely.
    async fn has_limits(&self, customer_ref: &str) -> Result<bool, LimitsProviderError>;
}
