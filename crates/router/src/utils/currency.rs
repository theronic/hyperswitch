use std::{
    collections::HashMap,
    ops::Deref,
    str::FromStr,
    sync::{Arc, LazyLock},
};

use api_models::enums;
use common_utils::{date_time, errors::CustomResult, events::ApiEventMetric, ext_traits::AsyncExt};
use currency_conversion::types::{CurrencyFactors, ExchangeRates};
use error_stack::ResultExt;
use hyperswitch_masking::{PeekInterface, Secret};
use redis_interface::DelReply;
use router_env::{instrument, tracing};
use rust_decimal::Decimal;
use strum::IntoEnumIterator;
use tokio::sync::RwLock;
use tracing_futures::Instrument;

use crate::{
    logger,
    routes::app::settings::{self, Conversion, DefaultExchangeRates, ProviderName},
    services, SessionState,
};
const REDIX_FOREX_CACHE_KEY: &str = "{forex_cache}_lock";
const REDIX_FOREX_CACHE_DATA: &str = "{forex_cache}_data";
const FOREX_API_TIMEOUT: u64 = 5;
const OER_BASE_URL: &str = "https://openexchangerates.org/api/latest.json?app_id=";
const OER_BASE_PARAM: &str = "&base=USD";
const CURRENCY_LAYER_BASE_URL: &str = "http://apilayer.net/api/live?access_key=";
const CURRENCY_LAYER_QUOTE_PREFIX: &str = "USD";
const FIXER_BASE_URL: &str = "https://data.fixer.io/api/latest?access_key=";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FxExchangeRatesCacheEntry {
    pub data: Arc<ExchangeRates>,
    timestamp: i64,
}

static FX_EXCHANGE_RATES_CACHE: LazyLock<RwLock<Option<FxExchangeRatesCacheEntry>>> =
    LazyLock::new(|| RwLock::new(None));

impl ApiEventMetric for FxExchangeRatesCacheEntry {}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ForexError {
    #[error("API error")]
    ApiError,
    #[error("API timeout")]
    ApiTimeout,
    #[error("API unresponsive")]
    ApiUnresponsive,
    #[error("Conversion error")]
    ConversionError,
    #[error("Could not acquire the lock for cache entry")]
    CouldNotAcquireLock,
    #[error("Provided currency not acceptable")]
    CurrencyNotAcceptable,
    #[error("Forex configuration error: {0}")]
    ConfigurationError(String),
    #[error("Incorrect entries in default Currency response")]
    DefaultCurrencyParsingError,
    #[error("Entry not found in cache")]
    EntryNotFound,
    #[error("Forex data unavailable")]
    ForexDataUnavailable,
    #[error("Expiration time invalid")]
    InvalidLogExpiry,
    #[error("Error reading local")]
    LocalReadError,
    #[error("Error writing to local cache")]
    LocalWriteError,
    #[error("Json Parsing error")]
    ParsingError,
    #[error("Aws Kms decryption error")]
    AwsKmsDecryptionFailed,
    #[error("Error connecting to redis")]
    RedisConnectionError,
    #[error("Not able to release write lock")]
    RedisLockReleaseFailed,
    #[error("Error writing to redis")]
    RedisWriteError,
    #[error("Not able to acquire write lock")]
    WriteLockNotAcquired,
}

#[derive(Debug, serde::Deserialize)]
struct OerResponse {
    rates: HashMap<String, FloatDecimal>,
}

impl OerResponse {
    fn into_exchange_rates(self) -> ExchangeRates {
        build_exchange_rates(enums::Currency::USD, |enum_curr| {
            self.rates.get(&enum_curr.to_string()).map(|rate| **rate)
        })
    }
}

#[derive(Debug, serde::Deserialize)]
struct CurrencyLayerResponse {
    quotes: HashMap<String, FloatDecimal>,
}

impl CurrencyLayerResponse {
    fn into_exchange_rates(self) -> ExchangeRates {
        // currencylayer quote keys are prefixed with the source currency, e.g. `USDZAR`.
        build_exchange_rates(enums::Currency::USD, |enum_curr| {
            self.quotes
                .get(&format!("{CURRENCY_LAYER_QUOTE_PREFIX}{enum_curr}"))
                .map(|rate| **rate)
        })
    }
}

// data.fixer.io returns `{ "success": true, "base": .., "rates": { .. } }` on success or
// `{ "success": false, "error": { .. } }` on failure. The `success` flag is parsed
// explicitly so an unexpected body yields a precise error rather than an opaque one.
#[derive(Debug, serde::Deserialize)]
struct FixerResponse {
    success: bool,
    base: Option<String>,
    rates: Option<HashMap<String, FloatDecimal>>,
    error: Option<FixerErrorBody>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FixerErrorBody {
    code: i64,
    #[serde(rename = "type")]
    error_type: String,
    info: Option<String>,
}

impl FixerResponse {
    fn into_exchange_rates(self) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
        if !self.success {
            let error = self.error.unwrap_or_default();
            logger::error!(
                "forex_error: Fixer api returned an error (code {}, {}): {:?}",
                error.code,
                error.error_type,
                error.info,
            );
            return Err(ForexError::ApiError.into());
        }
        // A success body with no rates is anomalous; fail over rather than cache an
        // exchange-rates table that can only resolve the base currency.
        let rates = match self.rates {
            Some(rates) if !rates.is_empty() => rates,
            _ => {
                logger::error!("forex_error: Fixer success response contained no rates");
                return Err(ForexError::ApiError.into());
            }
        };
        let base = self
            .base
            .ok_or(ForexError::ApiError)
            .attach_printable("Fixer success response missing base currency")?;
        let base_currency = enums::Currency::from_str(base.as_str())
            .change_context(ForexError::ConversionError)
            .attach_printable("Unable to convert base currency received from fixer api")?;
        Ok(build_exchange_rates(base_currency, |enum_curr| {
            rates.get(&enum_curr.to_string()).map(|rate| **rate)
        }))
    }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct FloatDecimal(#[serde(with = "rust_decimal::serde::float")] Decimal);

impl Deref for FloatDecimal {
    type Target = Decimal;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FxExchangeRatesCacheEntry {
    fn new(exchange_rate: ExchangeRates) -> Self {
        Self {
            data: Arc::new(exchange_rate),
            timestamp: date_time::now_unix_timestamp(),
        }
    }
    fn is_expired(&self, data_expiration_delay: u32) -> bool {
        self.timestamp + i64::from(data_expiration_delay) < date_time::now_unix_timestamp()
    }
}

async fn retrieve_forex_from_local_cache() -> Option<FxExchangeRatesCacheEntry> {
    FX_EXCHANGE_RATES_CACHE.read().await.clone()
}

async fn save_forex_data_to_local_cache(
    exchange_rates_cache_entry: FxExchangeRatesCacheEntry,
) -> CustomResult<(), ForexError> {
    let mut local = FX_EXCHANGE_RATES_CACHE.write().await;
    *local = Some(exchange_rates_cache_entry);
    logger::debug!("forex_log: forex saved in cache");
    Ok(())
}

impl TryFrom<DefaultExchangeRates> for ExchangeRates {
    type Error = error_stack::Report<ForexError>;
    fn try_from(value: DefaultExchangeRates) -> Result<Self, Self::Error> {
        let mut conversion_usable: HashMap<enums::Currency, CurrencyFactors> = HashMap::new();
        for (curr, conversion) in value.conversion {
            let enum_curr = enums::Currency::from_str(curr.as_str())
                .change_context(ForexError::ConversionError)
                .attach_printable("Unable to Convert currency received")?;
            conversion_usable.insert(enum_curr, CurrencyFactors::from(conversion));
        }
        let base_curr = enums::Currency::from_str(value.base_currency.as_str())
            .change_context(ForexError::ConversionError)
            .attach_printable("Unable to convert base currency")?;
        Ok(Self {
            base_currency: base_curr,
            conversion: conversion_usable,
        })
    }
}

impl From<Conversion> for CurrencyFactors {
    fn from(value: Conversion) -> Self {
        Self {
            to_factor: value.to_factor,
            from_factor: value.from_factor,
        }
    }
}

#[instrument(skip_all)]
pub async fn get_forex_rates(
    state: &SessionState,
    data_expiration_delay: u32,
) -> CustomResult<FxExchangeRatesCacheEntry, ForexError> {
    if let Some(local_rates) = retrieve_forex_from_local_cache().await {
        if local_rates.is_expired(data_expiration_delay) {
            // expired local data
            logger::debug!("forex_log: Forex stored in cache is expired");
            call_forex_api_and_save_data_to_cache_and_redis(state, Some(local_rates)).await
        } else {
            // Valid data present in local
            logger::debug!("forex_log: forex found in cache");
            Ok(local_rates)
        }
    } else {
        // No data in local
        call_api_if_redis_forex_data_expired(state, data_expiration_delay).await
    }
}

async fn call_api_if_redis_forex_data_expired(
    state: &SessionState,
    data_expiration_delay: u32,
) -> CustomResult<FxExchangeRatesCacheEntry, ForexError> {
    match retrieve_forex_data_from_redis(state).await {
        Ok(Some(data)) => {
            call_forex_api_if_redis_data_expired(state, data, data_expiration_delay).await
        }
        Ok(None) => {
            // No data in local as well as redis
            call_forex_api_and_save_data_to_cache_and_redis(state, None).await?;
            Err(ForexError::ForexDataUnavailable.into())
        }
        Err(error) => {
            // Error in deriving forex rates from redis
            logger::error!("forex_error: {:?}", error);
            call_forex_api_and_save_data_to_cache_and_redis(state, None).await?;
            Err(ForexError::ForexDataUnavailable.into())
        }
    }
}

async fn call_forex_api_and_save_data_to_cache_and_redis(
    state: &SessionState,
    stale_redis_data: Option<FxExchangeRatesCacheEntry>,
) -> CustomResult<FxExchangeRatesCacheEntry, ForexError> {
    // spawn a new thread and do the api fetch and write operations on redis.
    let forex_api = state.conf.forex_api.get_inner();
    // Hard-error only when neither the primary nor the fallback has a key. With a configured
    // fallback, an empty primary key must still fail over (handled by `fetch_with_fallback`),
    // not short-circuit here.
    if forex_api.key_for(forex_api.provider).peek().is_empty()
        && forex_api
            .key_for(forex_api.fallback_provider)
            .peek()
            .is_empty()
    {
        Err(ForexError::ConfigurationError("api_keys not provided".into()).into())
    } else {
        let state = state.clone();
        tokio::spawn(
            async move {
                acquire_redis_lock_and_call_forex_api(&state)
                    .await
                    .map_err(|err| {
                        logger::error!(forex_error=?err);
                    })
                    .ok();
            }
            .in_current_span(),
        );
        stale_redis_data.ok_or(ForexError::EntryNotFound.into())
    }
}

async fn acquire_redis_lock_and_call_forex_api(
    state: &SessionState,
) -> CustomResult<(), ForexError> {
    let lock_acquired = acquire_redis_lock(state).await?;
    if !lock_acquired {
        Err(ForexError::CouldNotAcquireLock.into())
    } else {
        logger::debug!("forex_log: redis lock acquired");
        let forex_api = state.conf.forex_api.get_inner();
        let primary = ForexProvider::from_config(forex_api.provider, forex_api, state.clone());
        let fallback =
            ForexProvider::from_config(forex_api.fallback_provider, forex_api, state.clone());
        match fetch_with_fallback(&primary, &fallback).await {
            Ok(rates) => {
                save_forex_data_to_cache_and_redis(state, FxExchangeRatesCacheEntry::new(rates))
                    .await
            }
            Err(error) => {
                release_redis_lock(state).await?;
                Err(error)
            }
        }
    }
}

async fn save_forex_data_to_cache_and_redis(
    state: &SessionState,
    forex: FxExchangeRatesCacheEntry,
) -> CustomResult<(), ForexError> {
    save_forex_data_to_redis(state, &forex)
        .await
        .async_and_then(|_rates| release_redis_lock(state))
        .await
        .async_and_then(|_val| save_forex_data_to_local_cache(forex.clone()))
        .await
}

async fn call_forex_api_if_redis_data_expired(
    state: &SessionState,
    redis_data: FxExchangeRatesCacheEntry,
    data_expiration_delay: u32,
) -> CustomResult<FxExchangeRatesCacheEntry, ForexError> {
    match is_redis_expired(Some(redis_data.clone()).as_ref(), data_expiration_delay).await {
        Some(redis_forex) => {
            // Valid data present in redis
            let exchange_rates = FxExchangeRatesCacheEntry::new(redis_forex.as_ref().clone());
            logger::debug!("forex_log: forex response found in redis");
            save_forex_data_to_local_cache(exchange_rates.clone()).await?;
            Ok(exchange_rates)
        }
        None => {
            // redis expired
            call_forex_api_and_save_data_to_cache_and_redis(state, Some(redis_data)).await
        }
    }
}

/// Builds `ExchangeRates` from a base currency and a `base -> currency` rate lookup.
/// `rate_of(c)` returns the units of `c` per one unit of `base`. The base currency is
/// always inserted as identity `(1, 1)` so base conversions are defined even when a feed
/// omits the base from its table. Non-invertible (zero) rates are skipped.
fn build_exchange_rates(
    base_currency: enums::Currency,
    rate_of: impl Fn(enums::Currency) -> Option<Decimal>,
) -> ExchangeRates {
    let mut conversions: HashMap<enums::Currency, CurrencyFactors> = HashMap::new();
    for enum_curr in enums::Currency::iter() {
        let rate = match rate_of(enum_curr) {
            Some(rate) => rate,
            None if enum_curr == base_currency => Decimal::new(1, 0),
            None => continue,
        };
        match Decimal::new(1, 0).checked_div(rate) {
            Some(from_factor) => {
                conversions.insert(enum_curr, CurrencyFactors::new(rate, from_factor));
            }
            None => {
                logger::warn!(
                    "forex_log: zero/non-invertible rate for {}, skipped",
                    enum_curr
                )
            }
        }
    }
    // Missing currencies are skipped silently (a feed need not quote every ISO code), so log
    // the resulting coverage — a sudden drop in the count signals a truncated feed.
    logger::debug!(
        "forex_log: built {} conversion rates for base {}",
        conversions.len(),
        base_currency
    );
    ExchangeRates::new(base_currency, conversions)
}

/// Shared HTTP GET + JSON parse for forex providers. Logs only the provider name —
/// never the URL, which carries the api key as a query parameter.
async fn forex_get_json<R>(
    state: &SessionState,
    provider: &str,
    url: &str,
) -> Result<R, error_stack::Report<ForexError>>
where
    R: serde::de::DeserializeOwned,
{
    logger::debug!(forex_provider = provider, "forex_log: fetching forex rates");
    let request = services::RequestBuilder::new()
        .method(services::Method::Get)
        .url(url)
        .build();
    let response = state
        .api_client
        .send_request(&state.clone(), request, Some(FOREX_API_TIMEOUT), false)
        .await
        .change_context(ForexError::ApiUnresponsive)
        .attach_printable("forex fetch api unresponsive")?;
    response
        .json::<R>()
        .await
        .change_context(ForexError::ParsingError)
        .attach_printable("Unable to parse response received from forex api")
}

/// Abstract interface for a forex rate provider: fetches the full rate table and normalizes
/// it to base-agnostic `ExchangeRates`. Implemented by each concrete provider and by the
/// wrapping `ForexProvider` enum, so the failover seam (`fetch_with_fallback`) can be
/// unit-tested with a mock — no `SessionState` required.
#[async_trait::async_trait]
trait ForexRateProvider {
    /// Stable label for logs/metrics. MUST NOT contain the credential.
    fn name(&self) -> &'static str;

    /// Fetch the full rate table and normalize to base-agnostic `ExchangeRates`.
    async fn fetch_rates(&self) -> Result<ExchangeRates, error_stack::Report<ForexError>>;
}

/// openexchangerates.org — USD base.
struct OpenExchangeRates {
    api_key: Secret<String>,
    state: SessionState,
}

/// data.fixer.io — EUR base on the paid plan; the base is read from the response.
struct Fixer {
    api_key: Secret<String>,
    state: SessionState,
}

/// apilayer.net/api/live — USD base; quote keys are prefixed with `USD`.
struct CurrencyLayer {
    api_key: Secret<String>,
    state: SessionState,
}

#[async_trait::async_trait]
impl ForexRateProvider for OpenExchangeRates {
    fn name(&self) -> &'static str {
        "open_exchange_rates"
    }
    async fn fetch_rates(&self) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
        let url = format!("{OER_BASE_URL}{}{OER_BASE_PARAM}", self.api_key.peek());
        let response: OerResponse = forex_get_json(&self.state, self.name(), &url).await?;
        Ok(response.into_exchange_rates())
    }
}

#[async_trait::async_trait]
impl ForexRateProvider for Fixer {
    fn name(&self) -> &'static str {
        "fixer"
    }
    async fn fetch_rates(&self) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
        let url = format!("{FIXER_BASE_URL}{}", self.api_key.peek());
        let response: FixerResponse = forex_get_json(&self.state, self.name(), &url).await?;
        response.into_exchange_rates()
    }
}

#[async_trait::async_trait]
impl ForexRateProvider for CurrencyLayer {
    fn name(&self) -> &'static str {
        "currency_layer"
    }
    async fn fetch_rates(&self) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
        let url = format!("{CURRENCY_LAYER_BASE_URL}{}", self.api_key.peek());
        let response: CurrencyLayerResponse =
            forex_get_json(&self.state, self.name(), &url).await?;
        Ok(response.into_exchange_rates())
    }
}

/// Closed set of forex providers, dispatched by variant (static dispatch; no trait object).
enum ForexProvider {
    OpenExchangeRates(OpenExchangeRates),
    Fixer(Fixer),
    CurrencyLayer(CurrencyLayer),
}

impl ForexProvider {
    /// Construct the active provider from (already-decrypted) configuration. `state` is cloned
    /// into the provider so each fetch carries its own context and the failover seam stays
    /// mockable without a `SessionState`.
    fn from_config(name: ProviderName, conf: &settings::ForexApi, state: SessionState) -> Self {
        let api_key = conf.key_for(name).clone();
        match name {
            ProviderName::OpenExchangeRates => {
                Self::OpenExchangeRates(OpenExchangeRates { api_key, state })
            }
            ProviderName::Fixer => Self::Fixer(Fixer { api_key, state }),
            ProviderName::CurrencyLayer => Self::CurrencyLayer(CurrencyLayer { api_key, state }),
        }
    }
}

#[async_trait::async_trait]
impl ForexRateProvider for ForexProvider {
    fn name(&self) -> &'static str {
        match self {
            Self::OpenExchangeRates(provider) => provider.name(),
            Self::Fixer(provider) => provider.name(),
            Self::CurrencyLayer(provider) => provider.name(),
        }
    }
    async fn fetch_rates(&self) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
        match self {
            Self::OpenExchangeRates(provider) => provider.fetch_rates().await,
            Self::Fixer(provider) => provider.fetch_rates().await,
            Self::CurrencyLayer(provider) => provider.fetch_rates().await,
        }
    }
}

/// Fetch from `primary`, falling back to `fallback` on any error. When *both* fail, a single
/// `forex_all_providers_failed` line names both providers and the fallback's error is annotated
/// with the cross-provider context, so the total failure is not anonymous in the returned report.
async fn fetch_with_fallback(
    primary: &impl ForexRateProvider,
    fallback: &impl ForexRateProvider,
) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
    match primary.fetch_rates().await {
        Ok(rates) => Ok(rates),
        Err(primary_error) => {
            logger::error!(forex_error = ?primary_error, primary = primary.name(), "primary_forex_error");
            fallback.fetch_rates().await.map_err(|fallback_error| {
                logger::error!(
                    primary = primary.name(),
                    fallback = fallback.name(),
                    forex_error = ?fallback_error,
                    "forex_all_providers_failed: primary and fallback both failed"
                );
                fallback_error.attach_printable(format!(
                    "fallback provider `{}` also failed after primary `{}`",
                    fallback.name(),
                    primary.name(),
                ))
            })
        }
    }
}

async fn release_redis_lock(
    state: &SessionState,
) -> Result<DelReply, error_stack::Report<ForexError>> {
    logger::debug!("forex_log: Releasing redis lock");
    state
        .store
        .get_redis_conn()
        .change_context(ForexError::RedisConnectionError)?
        .delete_key(&REDIX_FOREX_CACHE_KEY.into())
        .await
        .change_context(ForexError::RedisLockReleaseFailed)
        .attach_printable("Unable to release redis lock")
}

async fn acquire_redis_lock(state: &SessionState) -> CustomResult<bool, ForexError> {
    let forex_api = state.conf.forex_api.get_inner();
    logger::debug!("forex_log: Acquiring redis lock");
    state
        .store
        .get_redis_conn()
        .change_context(ForexError::RedisConnectionError)?
        .set_key_if_not_exists_with_expiry(
            &REDIX_FOREX_CACHE_KEY.into(),
            "",
            Some(i64::from(forex_api.redis_lock_timeout_in_seconds)),
        )
        .await
        .map(|val| matches!(val, redis_interface::SetnxReply::KeySet))
        .change_context(ForexError::CouldNotAcquireLock)
        .attach_printable("Unable to acquire redis lock")
}

async fn save_forex_data_to_redis(
    app_state: &SessionState,
    forex_exchange_cache_entry: &FxExchangeRatesCacheEntry,
) -> CustomResult<(), ForexError> {
    let forex_api = app_state.conf.forex_api.get_inner();
    logger::debug!("forex_log: Saving forex to redis");
    app_state
        .store
        .get_redis_conn()
        .change_context(ForexError::RedisConnectionError)?
        .serialize_and_set_key_with_expiry(
            &REDIX_FOREX_CACHE_DATA.into(),
            forex_exchange_cache_entry,
            i64::from(forex_api.redis_ttl_in_seconds),
        )
        .await
        .change_context(ForexError::RedisWriteError)
        .attach_printable("Unable to save forex data to redis")
}

async fn retrieve_forex_data_from_redis(
    app_state: &SessionState,
) -> CustomResult<Option<FxExchangeRatesCacheEntry>, ForexError> {
    logger::debug!("forex_log: Retrieving forex from redis");
    app_state
        .store
        .get_redis_conn()
        .change_context(ForexError::RedisConnectionError)?
        .get_and_deserialize_key(&REDIX_FOREX_CACHE_DATA.into(), "FxExchangeRatesCache")
        .await
        .change_context(ForexError::EntryNotFound)
        .attach_printable("Forex entry not found in redis")
}

async fn is_redis_expired(
    redis_cache: Option<&FxExchangeRatesCacheEntry>,
    data_expiration_delay: u32,
) -> Option<Arc<ExchangeRates>> {
    redis_cache.and_then(|cache| {
        if cache.timestamp + i64::from(data_expiration_delay) > date_time::now_unix_timestamp() {
            Some(cache.data.clone())
        } else {
            logger::debug!("forex_log: Forex stored in redis is expired");
            None
        }
    })
}

#[instrument(skip_all)]
pub async fn convert_currency(
    state: SessionState,
    amount: i64,
    to_currency: String,
    from_currency: String,
) -> CustomResult<api_models::currency::CurrencyConversionResponse, ForexError> {
    let forex_api = state.conf.forex_api.get_inner();
    let rates = get_forex_rates(&state, forex_api.data_expiration_delay_in_seconds)
        .await
        .change_context(ForexError::ApiError)?;

    let to_currency = enums::Currency::from_str(to_currency.as_str())
        .change_context(ForexError::CurrencyNotAcceptable)
        .attach_printable("The provided currency is not acceptable")?;

    let from_currency = enums::Currency::from_str(from_currency.as_str())
        .change_context(ForexError::CurrencyNotAcceptable)
        .attach_printable("The provided currency is not acceptable")?;

    let converted_amount =
        currency_conversion::conversion::convert(&rates.data, from_currency, to_currency, amount)
            .change_context(ForexError::ConversionError)
            .attach_printable("Unable to perform currency conversion")?;

    Ok(api_models::currency::CurrencyConversionResponse {
        converted_amount: converted_amount.to_string(),
        currency: to_currency.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd(mantissa: i64, scale: u32) -> FloatDecimal {
        FloatDecimal(Decimal::new(mantissa, scale))
    }

    #[test]
    fn provider_name_deserializes_each_variant() {
        assert_eq!(
            serde_json::from_str::<ProviderName>("\"open_exchange_rates\"").unwrap(),
            ProviderName::OpenExchangeRates
        );
        assert_eq!(
            serde_json::from_str::<ProviderName>("\"fixer\"").unwrap(),
            ProviderName::Fixer
        );
        assert_eq!(
            serde_json::from_str::<ProviderName>("\"currency_layer\"").unwrap(),
            ProviderName::CurrencyLayer
        );
        assert!(serde_json::from_str::<ProviderName>("\"nope\"").is_err());
    }

    // Config deserialization gate: provider/fallback selectors + the flat key fields.
    #[test]
    fn forex_api_deserializes_config() {
        let conf: settings::ForexApi = serde_json::from_value(serde_json::json!({
            "provider": "fixer",
            "fallback_provider": "currency_layer",
            "api_key": "oer",
            "fallback_api_key": "cl",
            "fixer_api_key": "fx",
            "data_expiration_delay_in_seconds": 21600,
            "redis_lock_timeout_in_seconds": 100,
            "redis_ttl_in_seconds": 172800,
        }))
        .unwrap();
        assert_eq!(conf.provider, ProviderName::Fixer);
        assert_eq!(conf.fallback_provider, ProviderName::CurrencyLayer);
        // key_for maps each provider to its credential field.
        assert_eq!(conf.key_for(ProviderName::Fixer).peek(), "fx");
        assert_eq!(conf.key_for(ProviderName::OpenExchangeRates).peek(), "oer");
        assert_eq!(conf.key_for(ProviderName::CurrencyLayer).peek(), "cl");
    }

    // Backward compatibility: a pre-Fixer `[forex_api]` (only api_key + fallback_api_key,
    // no provider/fallback_provider/fixer_api_key) still deserializes and keeps the original
    // behaviour — OER primary, Currency Layer fallback — via the struct's serde defaults.
    #[test]
    fn legacy_forex_config_is_backward_compatible() {
        let conf: settings::ForexApi = serde_json::from_value(serde_json::json!({
            "api_key": "oer",
            "fallback_api_key": "cl",
            "data_expiration_delay_in_seconds": 21600,
            "redis_lock_timeout_in_seconds": 100,
            "redis_ttl_in_seconds": 172800,
        }))
        .unwrap();
        assert_eq!(conf.provider, ProviderName::OpenExchangeRates);
        assert_eq!(conf.fallback_provider, ProviderName::CurrencyLayer);
        assert_eq!(conf.key_for(ProviderName::OpenExchangeRates).peek(), "oer");
        assert_eq!(conf.key_for(ProviderName::CurrencyLayer).peek(), "cl");
        assert!(conf.fixer_api_key.peek().is_empty());
    }

    #[test]
    fn default_fallback_is_currency_layer() {
        let conf = settings::ForexApi::default();
        assert_eq!(conf.provider, ProviderName::OpenExchangeRates);
        assert_eq!(conf.fallback_provider, ProviderName::CurrencyLayer);
    }

    #[test]
    fn build_exchange_rates_sets_inverse_factors() {
        let rates = build_exchange_rates(enums::Currency::USD, |curr| match curr {
            enums::Currency::USD => Some(Decimal::new(1, 0)),
            enums::Currency::ZAR => Some(Decimal::new(185, 1)), // 18.5
            _ => None,
        });
        assert_eq!(rates.base_currency, enums::Currency::USD);
        let zar = rates.conversion.get(&enums::Currency::ZAR).unwrap();
        assert_eq!(zar.to_factor, Decimal::new(185, 1));
        assert_eq!(
            zar.from_factor,
            Decimal::new(1, 0)
                .checked_div(Decimal::new(185, 1))
                .unwrap()
        );
    }

    #[test]
    fn build_exchange_rates_inserts_base_identity_when_absent() {
        // EUR (the base) is not present in the feed.
        let rates = build_exchange_rates(enums::Currency::EUR, |curr| match curr {
            enums::Currency::ZAR => Some(Decimal::new(20, 0)),
            _ => None,
        });
        let eur = rates.conversion.get(&enums::Currency::EUR).unwrap();
        assert_eq!(eur.to_factor, Decimal::new(1, 0));
        assert_eq!(eur.from_factor, Decimal::new(1, 0));
    }

    #[test]
    fn build_exchange_rates_skips_zero_rate() {
        let rates = build_exchange_rates(enums::Currency::USD, |curr| match curr {
            enums::Currency::USD => Some(Decimal::new(1, 0)),
            enums::Currency::ZAR => Some(Decimal::new(0, 0)),
            _ => None,
        });
        assert!(!rates.conversion.contains_key(&enums::Currency::ZAR));
    }

    #[test]
    fn oer_response_uses_usd_base() {
        let response = OerResponse {
            rates: HashMap::from([
                ("USD".to_string(), fd(1, 0)),
                ("ZAR".to_string(), fd(185, 1)),
            ]),
        };
        let rates = response.into_exchange_rates();
        assert_eq!(rates.base_currency, enums::Currency::USD);
        assert!(rates.conversion.contains_key(&enums::Currency::ZAR));
    }

    #[test]
    fn currency_layer_strips_usd_prefix() {
        let response = CurrencyLayerResponse {
            quotes: HashMap::from([("USDZAR".to_string(), fd(185, 1))]),
        };
        let rates = response.into_exchange_rates();
        assert_eq!(rates.base_currency, enums::Currency::USD);
        assert!(rates.conversion.contains_key(&enums::Currency::ZAR));
    }

    #[test]
    fn fixer_success_reads_base_from_response() {
        let response = FixerResponse {
            success: true,
            base: Some("EUR".to_string()),
            rates: Some(HashMap::from([
                ("USD".to_string(), fd(11634, 4)),  // 1.1634
                ("ZAR".to_string(), fd(189115, 4)), // 18.9115
                ("EUR".to_string(), fd(1, 0)),
            ])),
            error: None,
        };
        let rates = response.into_exchange_rates().unwrap();
        assert_eq!(rates.base_currency, enums::Currency::EUR);
        assert!(rates.conversion.contains_key(&enums::Currency::USD));
    }

    #[test]
    fn fixer_error_body_is_an_error() {
        let response = FixerResponse {
            success: false,
            base: None,
            rates: None,
            error: Some(FixerErrorBody {
                code: 105,
                error_type: "base_currency_access_restricted".to_string(),
                info: None,
            }),
        };
        assert!(response.into_exchange_rates().is_err());
    }

    // R6: a success body with no rates must fail over, not cache a base-only table.
    #[test]
    fn fixer_success_without_rates_is_an_error() {
        let response = FixerResponse {
            success: true,
            base: Some("EUR".to_string()),
            rates: None,
            error: None,
        };
        assert!(response.into_exchange_rates().is_err());
    }

    // The correctness crown jewel: EUR-based rates convert every pair (incl. X->USD).
    #[test]
    fn conversion_is_base_agnostic_through_eur() {
        let response = FixerResponse {
            success: true,
            base: Some("EUR".to_string()),
            rates: Some(HashMap::from([
                ("USD".to_string(), fd(11634, 4)),  // 1 EUR = 1.1634 USD
                ("ZAR".to_string(), fd(189115, 4)), // 1 EUR = 18.9115 ZAR
                ("EUR".to_string(), fd(1, 0)),
            ])),
            error: None,
        };
        let rates = response.into_exchange_rates().unwrap();

        // 100.00 USD -> ZAR  (~ 100 * 18.9115 / 1.1634 = 1625.5)
        let usd_to_zar = currency_conversion::conversion::convert(
            &rates,
            enums::Currency::USD,
            enums::Currency::ZAR,
            10000,
        )
        .unwrap();
        assert!(
            usd_to_zar > Decimal::new(1600, 0) && usd_to_zar < Decimal::new(1650, 0),
            "USD->ZAR was {usd_to_zar}"
        );

        // 100.00 ZAR -> USD  (~ 6.15) — exercises the X->USD path analytics relies on.
        let zar_to_usd = currency_conversion::conversion::convert(
            &rates,
            enums::Currency::ZAR,
            enums::Currency::USD,
            10000,
        )
        .unwrap();
        assert!(
            zar_to_usd > Decimal::new(5, 0) && zar_to_usd < Decimal::new(7, 0),
            "ZAR->USD was {zar_to_usd}"
        );
    }

    // --- Failover orchestration: exercised via a mock (no SessionState needed). ---

    struct MockProvider {
        name: &'static str,
        result: Result<ExchangeRates, ForexError>,
    }

    #[async_trait::async_trait]
    impl ForexRateProvider for MockProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn fetch_rates(&self) -> Result<ExchangeRates, error_stack::Report<ForexError>> {
            self.result.clone().map_err(Into::into)
        }
    }

    fn rates_with_base(base: enums::Currency) -> ExchangeRates {
        build_exchange_rates(base, |curr| {
            if curr == base {
                Some(Decimal::new(1, 0))
            } else {
                None
            }
        })
    }

    #[tokio::test]
    async fn fallback_not_used_when_primary_succeeds() {
        let primary = MockProvider {
            name: "primary",
            result: Ok(rates_with_base(enums::Currency::USD)),
        };
        let fallback = MockProvider {
            name: "fallback",
            result: Ok(rates_with_base(enums::Currency::EUR)),
        };
        // USD base proves the primary's result was returned, not the fallback's EUR.
        let rates = fetch_with_fallback(&primary, &fallback).await.unwrap();
        assert_eq!(rates.base_currency, enums::Currency::USD);
    }

    #[tokio::test]
    async fn fallback_used_when_primary_fails() {
        let primary = MockProvider {
            name: "primary",
            result: Err(ForexError::ApiError),
        };
        let fallback = MockProvider {
            name: "fallback",
            result: Ok(rates_with_base(enums::Currency::EUR)),
        };
        let rates = fetch_with_fallback(&primary, &fallback).await.unwrap();
        assert_eq!(rates.base_currency, enums::Currency::EUR);
    }

    #[tokio::test]
    async fn error_when_both_providers_fail() {
        let primary = MockProvider {
            name: "primary",
            result: Err(ForexError::ApiError),
        };
        let fallback = MockProvider {
            name: "fallback",
            result: Err(ForexError::ApiUnresponsive),
        };
        let err = fetch_with_fallback(&primary, &fallback).await.unwrap_err();
        // The returned report carries cross-provider context (see `forex_all_providers_failed`).
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("fallback provider")
                && rendered.contains("also failed after primary"),
            "missing cross-provider context in both-failed error: {rendered}"
        );
    }
}
