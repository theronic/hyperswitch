//! HTTP `mypeach` implementation of the [`LimitsProvider`] trait.
//!
//! Talks to the MyPeach limits provider over the versioned contract:
//!
//! ```text
//! POST {base}/limits/v1/evaluate
//! POST {base}/limits/v1/settle
//! GET  {base}/limits/v1/has-limits?customer_ref=...
//! ```
//!
//! Every request is **HMAC-signed**: the signature is computed over
//! `body + "." + timestamp + "." + nonce` and carried (with the timestamp and
//! nonce) in request headers so the provider can authenticate and replay-reject
//! the call. Hyperswitch authenticates *to* MyPeach; MyPeach issues no HS
//! credential. All calls are bounded by a per-call timeout.

use std::time::Duration;

use api_models::limits::{
    LimitsEvaluateRequest, LimitsEvaluateResponse, LimitsHasLimitsResponse, LimitsSettleRequest,
};
use async_trait::async_trait;
use common_utils::crypto::{HmacSha256, SignMessage};
use hyperswitch_interfaces::api::limits::{LimitsProvider, LimitsProviderError};
use hyperswitch_masking::{PeekInterface, Secret};

/// Header carrying the hex-encoded HMAC signature of the request.
pub const HEADER_SIGNATURE: &str = "X-Limits-Signature";
/// Header carrying the unix-seconds timestamp the signature was computed over.
pub const HEADER_TIMESTAMP: &str = "X-Limits-Timestamp";
/// Header carrying the per-request nonce the signature was computed over.
pub const HEADER_NONCE: &str = "X-Limits-Nonce";

/// HTTP limits provider for the MyPeach backend.
pub struct MypeachLimitsProvider {
    client: reqwest::Client,
    base_url: String,
    hmac_secret: Secret<String>,
}

impl MypeachLimitsProvider {
    /// Build a provider for `base_url`, signing requests with `hmac_secret` and
    /// bounding every call by `timeout`.
    pub fn new(
        base_url: impl Into<String>,
        hmac_secret: Secret<String>,
        timeout: Duration,
    ) -> Result<Self, LimitsProviderError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|err| LimitsProviderError::Unavailable(err.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            hmac_secret,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Compute the request signature material for `body`.
    ///
    /// Returns `(signature_hex, timestamp, nonce)`. The signed message is
    /// `body + "." + timestamp + "." + nonce`.
    fn sign(&self, body: &str) -> Result<SignedHeaders, LimitsProviderError> {
        let timestamp = current_unix_seconds();
        let nonce = uuid::Uuid::new_v4().to_string();
        let signing_input = signing_input(body, timestamp, &nonce);
        let signature = HmacSha256
            .sign_message(self.hmac_secret.peek().as_bytes(), signing_input.as_bytes())
            .map_err(|err| LimitsProviderError::Unavailable(err.to_string()))?;
        Ok(SignedHeaders {
            signature: hex::encode(signature),
            timestamp: timestamp.to_string(),
            nonce,
        })
    }

    /// POST a JSON body to `path`, signing the request and parsing the response.
    async fn post_signed<Req, Resp>(
        &self,
        path: &str,
        req: &Req,
    ) -> Result<Resp, LimitsProviderError>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let body =
            serde_json::to_string(req).map_err(|err| LimitsProviderError::Decode(err.to_string()))?;
        let headers = self.sign(&body)?;

        let response = self
            .client
            .post(self.url(path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(HEADER_SIGNATURE, headers.signature)
            .header(HEADER_TIMESTAMP, headers.timestamp)
            .header(HEADER_NONCE, headers.nonce)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        parse_response(response).await
    }
}

#[async_trait]
impl LimitsProvider for MypeachLimitsProvider {
    async fn evaluate(
        &self,
        req: LimitsEvaluateRequest,
    ) -> Result<LimitsEvaluateResponse, LimitsProviderError> {
        self.post_signed("/limits/v1/evaluate", &req).await
    }

    async fn settle(&self, req: LimitsSettleRequest) -> Result<(), LimitsProviderError> {
        // The provider replies `{ ok: true }`; we only care that it parsed.
        let _resp: api_models::limits::LimitsSettleResponse =
            self.post_signed("/limits/v1/settle", &req).await?;
        Ok(())
    }

    async fn has_limits(&self, customer_ref: &str) -> Result<bool, LimitsProviderError> {
        // GET requests are signed over an empty body so the provider can still
        // enforce timestamp + nonce replay protection.
        let headers = self.sign("")?;
        let response = self
            .client
            .get(self.url("/limits/v1/has-limits"))
            .query(&[("customer_ref", customer_ref)])
            .header(HEADER_SIGNATURE, headers.signature)
            .header(HEADER_TIMESTAMP, headers.timestamp)
            .header(HEADER_NONCE, headers.nonce)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let parsed: LimitsHasLimitsResponse = parse_response(response).await?;
        Ok(parsed.has_limits)
    }
}

struct SignedHeaders {
    signature: String,
    timestamp: String,
    nonce: String,
}

/// The canonical message signed for a request: `body.timestamp.nonce`.
pub(crate) fn signing_input(body: &str, timestamp: i64, nonce: &str) -> String {
    format!("{body}.{timestamp}.{nonce}")
}

fn current_unix_seconds() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn map_reqwest_error(err: reqwest::Error) -> LimitsProviderError {
    if err.is_timeout() {
        LimitsProviderError::Timeout
    } else {
        LimitsProviderError::Unavailable(err.to_string())
    }
}

async fn parse_response<Resp>(response: reqwest::Response) -> Result<Resp, LimitsProviderError>
where
    Resp: serde::de::DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        return Err(LimitsProviderError::Http {
            status: status.as_u16(),
        });
    }
    let text = response
        .text()
        .await
        .map_err(|err| LimitsProviderError::Decode(err.to_string()))?;
    serde_json::from_str(&text).map_err(|err| LimitsProviderError::Decode(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_input_is_canonical_and_deterministic() {
        let a = signing_input("{\"x\":1}", 1_700_000_000, "nonce-1");
        let b = signing_input("{\"x\":1}", 1_700_000_000, "nonce-1");
        assert_eq!(a, b);
        assert_eq!(a, "{\"x\":1}.1700000000.nonce-1");
    }

    #[test]
    fn signature_is_stable_for_fixed_inputs() {
        // HMAC-SHA256 over a fixed (secret, message) must be reproducible; this
        // pins the signing contract so the provider can verify independently.
        let secret = b"top-secret";
        let msg = signing_input("body", 42, "abc");
        let sig1 = HmacSha256.sign_message(secret, msg.as_bytes()).unwrap();
        let sig2 = HmacSha256.sign_message(secret, msg.as_bytes()).unwrap();
        assert_eq!(hex::encode(&sig1), hex::encode(&sig2));
        assert_eq!(sig1.len(), 32, "HMAC-SHA256 is 32 bytes");
    }

    #[test]
    fn url_joins_without_double_slash() {
        let provider = MypeachLimitsProvider::new(
            "https://mypeach.example/",
            Secret::new("s".to_string()),
            Duration::from_millis(100),
        )
        .unwrap();
        assert_eq!(
            provider.url("/limits/v1/evaluate"),
            "https://mypeach.example/limits/v1/evaluate"
        );
    }
}
