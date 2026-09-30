//! Exact bounded decimal conversion; no floating-point estimate authorizes spend.
use super::*;
const SCALE: u128 = 1_000_000_000_000_000_000;

pub(super) fn decimal_units(text: &str) -> Result<u128, ProviderError> {
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
        if value % divisor != 0 {
            return Err(invalid("price exceeds exact decimal precision"));
        }
        Ok(value / divisor)
    }
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
pub(super) fn per_million_ceiling(per_token: &str) -> Result<String, ProviderError> {
    let units = decimal_units(per_token)?
        .checked_mul(1_000_000)
        .ok_or_else(|| invalid("price conversion overflow"))?;
    Ok(decimal_text(units))
}
pub(super) fn charge_bound(
    context: usize,
    output: usize,
    prompt: &str,
    completion: &str,
) -> Result<u64, ProviderError> {
    // rates are dollars / million tokens. USD -> microUSD cancels that million.
    let numerator = (context as u128)
        .checked_mul(decimal_units(prompt)?)
        .and_then(|p| {
            (output as u128)
                .checked_mul(decimal_units(completion).ok()?)
                .and_then(|c| p.checked_add(c))
        })
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
