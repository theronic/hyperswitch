use common_enums::Currency;
use rust_decimal::Decimal;
use rusty_money::Money;

use crate::{
    error::CurrencyConversionError,
    types::{currency_match, ExchangeRates},
};

pub fn convert(
    ex_rates: &ExchangeRates,
    from_currency: Currency,
    to_currency: Currency,
    amount: i64,
) -> Result<Decimal, CurrencyConversionError> {
    let money_minor = Money::from_minor(amount, currency_match(from_currency));
    let base_currency = ex_rates.base_currency;
    if to_currency == base_currency {
        ex_rates.forward_conversion(*money_minor.amount(), from_currency)
    } else if from_currency == base_currency {
        ex_rates.backward_conversion(*money_minor.amount(), to_currency)
    } else {
        let base_conversion_amt =
            ex_rates.forward_conversion(*money_minor.amount(), from_currency)?;
        ex_rates.backward_conversion(base_conversion_amt, to_currency)
    }
}

/// Convert `amount_minor` (minor units of `from_currency`) into the minor units
/// of `to_currency`, rounding **up** (ceil) to the next minor unit.
///
/// Ceil is deliberate: this backs agentic spending-limit enforcement, where a
/// converted charge must never be *undercounted* against a cap. Returns `None`
/// when the conversion is unavailable (missing rate / decimal overflow) or the
/// result overflows `i64`; callers fail-closed (block the transaction) on `None`.
pub fn to_minor_units_ceil(
    ex_rates: &ExchangeRates,
    from_currency: Currency,
    to_currency: Currency,
    amount_minor: i64,
) -> Option<i64> {
    if amount_minor < 0 {
        return None;
    }
    if from_currency == to_currency {
        return Some(amount_minor);
    }
    // `convert` returns the amount in MAJOR units of `to_currency` (unrounded).
    let major = convert(ex_rates, from_currency, to_currency, amount_minor).ok()?;
    let exponent = u32::from(to_currency.number_of_digits_after_decimal_point());
    let scale = Decimal::from(10u64.checked_pow(exponent)?);
    let minor = major.checked_mul(scale)?.ceil();
    let value = rust_decimal::prelude::ToPrimitive::to_i64(&minor)?;
    (value >= 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::types::CurrencyFactors;
    #[test]
    fn currency_to_currency_conversion() {
        use super::*;
        let mut conversion: HashMap<Currency, CurrencyFactors> = HashMap::new();
        let inr_conversion_rates =
            CurrencyFactors::new(Decimal::new(823173, 4), Decimal::new(1214, 5));
        let szl_conversion_rates =
            CurrencyFactors::new(Decimal::new(194423, 4), Decimal::new(514, 4));
        let convert_from = Currency::SZL;
        let convert_to = Currency::INR;
        let amount = 2000;
        let base_currency = Currency::USD;
        conversion.insert(convert_from, inr_conversion_rates);
        conversion.insert(convert_to, szl_conversion_rates);
        let sample_rate = ExchangeRates::new(base_currency, conversion);
        let res =
            convert(&sample_rate, convert_from, convert_to, amount).expect("converted_currency");
        println!("The conversion from {amount} {convert_from} to {convert_to} is {res:?}");
    }

    #[test]
    fn currency_to_base_conversion() {
        use super::*;
        let mut conversion: HashMap<Currency, CurrencyFactors> = HashMap::new();
        let inr_conversion_rates =
            CurrencyFactors::new(Decimal::new(823173, 4), Decimal::new(1214, 5));
        let usd_conversion_rates = CurrencyFactors::new(Decimal::new(1, 0), Decimal::new(1, 0));
        let convert_from = Currency::INR;
        let convert_to = Currency::USD;
        let amount = 2000;
        let base_currency = Currency::USD;
        conversion.insert(convert_from, inr_conversion_rates);
        conversion.insert(convert_to, usd_conversion_rates);
        let sample_rate = ExchangeRates::new(base_currency, conversion);
        let res =
            convert(&sample_rate, convert_from, convert_to, amount).expect("converted_currency");
        println!("The conversion from {amount} {convert_from} to {convert_to} is {res:?}");
    }

    #[test]
    fn base_to_currency_conversion() {
        use super::*;
        let mut conversion: HashMap<Currency, CurrencyFactors> = HashMap::new();
        let inr_conversion_rates =
            CurrencyFactors::new(Decimal::new(823173, 4), Decimal::new(1214, 5));
        let usd_conversion_rates = CurrencyFactors::new(Decimal::new(1, 0), Decimal::new(1, 0));
        let convert_from = Currency::USD;
        let convert_to = Currency::INR;
        let amount = 2000;
        let base_currency = Currency::USD;
        conversion.insert(convert_from, usd_conversion_rates);
        conversion.insert(convert_to, inr_conversion_rates);
        let sample_rate = ExchangeRates::new(base_currency, conversion);
        let res =
            convert(&sample_rate, convert_from, convert_to, amount).expect("converted_currency");
        println!("The conversion from {amount} {convert_from} to {convert_to} is {res:?}");
    }

    // ─── to_minor_units_ceil (agentic-limit FX rounding, plan v3 §6 / C2) ───

    /// USD-base rate table where `to` is reachable at `to_factor` (base -> to).
    fn usd_base_rates(to: Currency, to_factor: Decimal) -> ExchangeRates {
        use super::*;
        let mut conversion: HashMap<Currency, CurrencyFactors> = HashMap::new();
        conversion.insert(
            Currency::USD,
            CurrencyFactors::new(Decimal::new(1, 0), Decimal::new(1, 0)),
        );
        conversion.insert(to, CurrencyFactors::new(to_factor, Decimal::new(1, 0)));
        ExchangeRates::new(Currency::USD, conversion)
    }

    #[test]
    fn to_minor_same_currency_is_identity() {
        use super::*;
        let rates = usd_base_rates(Currency::ZAR, Decimal::new(18, 0));
        assert_eq!(
            to_minor_units_ceil(&rates, Currency::ZAR, Currency::ZAR, 12_345),
            Some(12_345)
        );
    }

    #[test]
    fn to_minor_two_decimal_exact() {
        use super::*;
        // USD $100.00 (10000 minor) -> ZAR @ 18.0 = R1800.00 = 180000 minor
        let rates = usd_base_rates(Currency::ZAR, Decimal::new(18, 0));
        assert_eq!(
            to_minor_units_ceil(&rates, Currency::USD, Currency::ZAR, 10_000),
            Some(180_000)
        );
    }

    #[test]
    fn to_minor_rounds_up_never_down() {
        use super::*;
        // USD $100.00 -> ZAR @ 18.00005 = R1800.005 -> 180000.5 minor -> ceil 180001
        let rates = usd_base_rates(Currency::ZAR, Decimal::new(1_800_005, 5));
        assert_eq!(
            to_minor_units_ceil(&rates, Currency::USD, Currency::ZAR, 10_000),
            Some(180_001)
        );
    }

    #[test]
    fn to_minor_zero_decimal_currency() {
        use super::*;
        // JPY is zero-decimal (minor == whole yen). $100 -> JPY @ 150.005 = ¥15000.5 -> ceil 15001
        let rates = usd_base_rates(Currency::JPY, Decimal::new(150_005, 3));
        assert_eq!(
            to_minor_units_ceil(&rates, Currency::USD, Currency::JPY, 10_000),
            Some(15_001)
        );
    }

    #[test]
    fn to_minor_missing_rate_is_none() {
        use super::*;
        let rates = usd_base_rates(Currency::ZAR, Decimal::new(18, 0));
        // KES rate absent -> conversion unavailable -> None (fail-closed)
        assert_eq!(
            to_minor_units_ceil(&rates, Currency::USD, Currency::KES, 10_000),
            None
        );
    }

    #[test]
    fn to_minor_overflow_is_none() {
        use super::*;
        // 6e17 USD minor * 18 * 100 exceeds i64::MAX -> None (fail-closed)
        let rates = usd_base_rates(Currency::ZAR, Decimal::new(18, 0));
        assert_eq!(
            to_minor_units_ceil(&rates, Currency::USD, Currency::ZAR, 600_000_000_000_000_000),
            None
        );
    }
}
