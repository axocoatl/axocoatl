//! Exact bounded decimal conversion; no floating-point estimate authorizes spend.
use super::*;
const SCALE: u128 = 1_000_000_000_000_000_000;

/// Parse a nonnegative decimal into units of 10^-18. With `round_up`, digits
/// beyond that precision raise the value to the next unit, which keeps a
/// ceiling conservative; without it they are refused as inexact.
fn units(text: &str, round_up: bool) -> Result<u128, ProviderError> {
    if text.is_empty() || text.len() > 96 || text.starts_with('-') || text.starts_with('+') {
        return Err(invalid("price is not a bounded nonnegative decimal"));
    }
    let (base, exponent) = match text.find(['e', 'E']) {
        Some(index) => (
            &text[..index],
            text[index + 1..]
                .parse::<i32>()
                .map_err(|_| invalid("invalid decimal exponent"))?,
        ),
        None => (text, 0),
    };
    let mut digits = String::new();
    let mut fraction = 0i32;
    let mut dot = false;
    for byte in base.bytes() {
        match byte {
            b'.' if !dot => dot = true,
            b'0'..=b'9' => {
                digits.push(byte as char);
                if dot {
                    fraction += 1;
                }
            }
            _ => return Err(invalid("invalid decimal price")),
        }
    }
    if digits.is_empty() {
        return Err(invalid("empty decimal price"));
    }
    let value = digits
        .parse::<u128>()
        .map_err(|_| invalid("price overflow"))?;
    let shift = 18i32
        .checked_add(exponent)
        .and_then(|n| n.checked_sub(fraction))
        .ok_or_else(|| invalid("price exponent overflow"))?;
    if !(-38..=38).contains(&shift) {
        return Err(invalid(
            "price precision outside supported decimal contract",
        ));
    }
    if shift >= 0 {
        value
            .checked_mul(10u128.pow(shift as u32))
            .ok_or_else(|| invalid("price overflow"))
    } else {
        let divisor = 10u128.pow((-shift) as u32);
        let whole = value / divisor;
        match (value % divisor, round_up) {
            (0, _) => Ok(whole),
            (_, true) => whole
                .checked_add(1)
                .ok_or_else(|| invalid("price overflow")),
            (_, false) => Err(invalid("price exceeds exact decimal precision")),
        }
    }
}

pub(super) fn decimal_units(text: &str) -> Result<u128, ProviderError> {
    units(text, false)
}

/// A catalog price as a ceiling: exact where it fits 18 decimals, otherwise
/// rounded up. Catalogs publish values such as `0.0000000416666666666667`.
pub(super) fn ceiling_units(text: &str) -> Result<u128, ProviderError> {
    units(text, true)
}

fn decimal_text(units: u128) -> String {
    let integer = units / SCALE;
    let fraction = units % SCALE;
    if fraction == 0 {
        integer.to_string()
    } else {
        format!("{integer}.{:018}", fraction)
            .trim_end_matches('0')
            .to_owned()
    }
}

/// OpenRouter's schema accepts decimal strings. Send exact decimal bytes so
/// serialization cannot introduce a floating-point change to the reservation.
pub(super) fn per_million_text(per_token_units: u128) -> Result<String, ProviderError> {
    Ok(decimal_text(
        per_token_units
            .checked_mul(1_000_000)
            .ok_or_else(|| invalid("price conversion overflow"))?,
    ))
}

/// An amount in units of 10^-18 as an exact decimal string.
pub(super) fn units_text(units: u128) -> String {
    decimal_text(units)
}

#[cfg(test)]
pub(super) fn per_million_ceiling(per_token: &str) -> Result<String, ProviderError> {
    per_million_text(decimal_units(per_token)?)
}

/// Whole-call charge ceiling in micro-USD, rounded up. Rates are dollars per
/// million tokens, so tokens times rate is already micro-dollars; the
/// per-request fee is in dollars.
pub(super) fn charge_bound(
    prompt_tokens: u64,
    response_tokens: u64,
    input_per_million: &str,
    output_per_million: &str,
    request_fee: Option<&str>,
) -> Result<u64, ProviderError> {
    let fee = request_fee
        .map(decimal_units)
        .transpose()?
        .unwrap_or(0)
        .checked_mul(1_000_000)
        .ok_or_else(|| invalid("request fee overflow"))?;
    let input = decimal_units(input_per_million)?;
    let output = decimal_units(output_per_million)?;
    let numerator = (prompt_tokens as u128)
        .checked_mul(input)
        .and_then(|p| {
            (response_tokens as u128)
                .checked_mul(output)
                .and_then(|c| p.checked_add(c))
        })
        .and_then(|tokens| tokens.checked_add(fee))
        .ok_or_else(|| invalid("price bound overflow"))?;
    let units = numerator
        .checked_add(SCALE - 1)
        .ok_or_else(|| invalid("price rounding overflow"))?
        / SCALE;
    u64::try_from(units).map_err(|_| invalid("price bound exceeds supported range"))
}

pub(super) fn measured_cost(value: &Value) -> Result<u64, ProviderError> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    let numerator = decimal_units(&text)?
        .checked_mul(1_000_000)
        .ok_or_else(|| invalid("measured cost overflow"))?;
    u64::try_from(
        numerator
            .checked_add(SCALE - 1)
            .ok_or_else(|| invalid("measured cost rounding overflow"))?
            / SCALE,
    )
    .map_err(|_| invalid("measured cost exceeds range"))
}
