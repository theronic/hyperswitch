//! Provider SLIs / decision counters for the limits connector.
//!
//! The decline-reason taxonomy (`limit_exceeded` / `fx_unavailable` /
//! `not_enabled` / `provider_unavailable`) is attached as the `reason`
//! attribute on [`LIMITS_BLOCKED`]; the provider-error taxonomy is attached as
//! `kind` on [`LIMITS_PROVIDER_ERROR`].

use router_env::{counter_metric, global_meter};

global_meter!(LIMITS_METER, "ROUTER_LIMITS");

counter_metric!(LIMITS_ALLOWED, LIMITS_METER); // evaluate -> allow
counter_metric!(LIMITS_BLOCKED, LIMITS_METER); // evaluate -> block (by `reason`)
counter_metric!(LIMITS_PROVIDER_ERROR, LIMITS_METER); // transport failure (by `kind`)
counter_metric!(LIMITS_FAIL_CLOSED, LIMITS_METER); // fail-mode resolved to block
counter_metric!(LIMITS_FAIL_OPEN, LIMITS_METER); // fail-mode resolved to allow
counter_metric!(LIMITS_BREAKER_OPEN, LIMITS_METER); // call skipped, breaker open
counter_metric!(LIMITS_FASTPATH_NO_LIMITS, LIMITS_METER); // has-limits=false short-circuit
counter_metric!(LIMITS_SKIPPED_NOT_TRIGGERED, LIMITS_METER); // trigger rule not met
counter_metric!(LIMITS_SETTLED, LIMITS_METER); // settle delivered (by `outcome`)
counter_metric!(LIMITS_SETTLE_FAILED, LIMITS_METER); // settle call failed (best-effort)
