use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAmount {
    pub currency: String,
    pub milli: i64,
}

pub fn parse_display_amount(text: &str) -> Option<ParsedAmount> {
    let first_digit = text.find(is_decimal_digit)?;
    let last_digit = text.rfind(is_decimal_digit)?;
    let prefix = &text[..first_digit];
    let suffix = text[last_digit..].trim_start_matches(is_decimal_digit);
    let currency_prefix = prefix.trim_end_matches(is_amount_separator);
    let currency_suffix = suffix.trim_start_matches(is_amount_separator);
    let currency = format!("{}{}", currency_prefix.trim(), currency_suffix.trim());
    let number_end = text.len() - currency_suffix.len();
    let number: String = text[currency_prefix.len()..number_end]
        .chars()
        .filter_map(|c| match c {
            '.' | '．' | '٫' => Some('.'),
            ',' | '，' => Some(','),
            _ => decimal_digit(c),
        })
        .collect();

    let decimal = decimal_position(&number);
    let mut milli = 0i64;
    let mut fractional_digits = 0;
    for (index, digit) in number.bytes().enumerate() {
        if !digit.is_ascii_digit() {
            continue;
        }
        let value = i64::from(digit - b'0');
        if decimal.is_some_and(|decimal| index > decimal) {
            fractional_digits += 1;
            if fractional_digits == 4 {
                milli = milli.saturating_add(i64::from(value >= 5));
                break;
            }
        }
        milli = milli.saturating_mul(10).saturating_add(value);
    }
    for _ in fractional_digits..3 {
        milli = milli.saturating_mul(10);
    }
    Some(ParsedAmount { currency, milli })
}

static DECIMAL_DIGIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A\p{Nd}\z").expect("decimal digit regex"));

fn is_decimal_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    DECIMAL_DIGIT.is_match(c.encode_utf8(&mut [0; 4]))
}

fn decimal_digit(c: char) -> Option<char> {
    if c.is_ascii_digit() {
        return Some(c);
    }
    if !is_decimal_digit(c) {
        return None;
    }
    // Unicode Nd digits occur in contiguous zero-to-nine groups.
    let mut block_start = c as u32;
    while char::from_u32(block_start - 1).is_some_and(is_decimal_digit) {
        block_start -= 1;
    }
    Some(char::from(b'0' + ((c as u32 - block_start) % 10) as u8))
}

fn is_amount_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '．' | '，' | '٫' | '٬' | '.' | ',' | '\'' | '’')
}

fn decimal_position(number: &str) -> Option<usize> {
    match (number.rfind('.'), number.rfind(',')) {
        (Some(dot), Some(comma)) => Some(dot.max(comma)),
        (Some(dot), None) if number.find('.') == Some(dot) => Some(dot),
        (None, Some(comma)) if number.find(',') == Some(comma) && number.len() - comma != 4 => {
            Some(comma)
        }
        _ => None,
    }
}

pub fn amount_from_micros(micros: i64, iso: &str) -> ParsedAmount {
    let remainder = micros % 1000;
    let milli = micros / 1000 + remainder.signum() * i64::from(remainder.abs() >= 500);
    ParsedAmount {
        currency: iso.trim().to_ascii_uppercase(),
        milli,
    }
}

pub fn normalize_currency(raw: &str, overrides: &BTreeMap<String, String>) -> String {
    let raw = raw.trim();
    if let Some(code) = overrides.get(raw) {
        return canonical_code(code.trim());
    }
    match raw {
        "¥" | "￥" | "JP¥" => "JPY".to_owned(),
        "$" | "US$" => "USD".to_owned(),
        "CA$" => "CAD".to_owned(),
        "A$" => "AUD".to_owned(),
        "€" => "EUR".to_owned(),
        "£" => "GBP".to_owned(),
        "₩" => "KRW".to_owned(),
        "NT$" => "TWD".to_owned(),
        "HK$" => "HKD".to_owned(),
        "₹" => "INR".to_owned(),
        _ => canonical_code(raw),
    }
}

fn canonical_code(raw: &str) -> String {
    if (raw.len() == 3 && raw.bytes().all(|b| b.is_ascii_alphabetic()))
        || raw.eq_ignore_ascii_case("pt")
        || raw.eq_ignore_ascii_case("bits")
    {
        raw.to_ascii_uppercase()
    } else {
        raw.to_owned()
    }
}

pub type RateTable = BTreeMap<String, f64>;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AmountTotals(pub BTreeMap<String, i64>);

impl AmountTotals {
    pub fn add(&mut self, raw_currency: &str, value: f64) {
        let milli = (value * 1000.0).round() as i64;
        if value.is_finite() && milli != 0 {
            let total = self.0.entry(raw_currency.to_owned()).or_default();
            *total = total.saturating_add(milli);
            if *total == 0 {
                self.0.remove(raw_currency);
            }
        }
    }

    pub fn jpy_milli(&self, rates: &RateTable, overrides: &BTreeMap<String, String>) -> i64 {
        let mut exact = 0i128;
        let mut converted = 0.0_f64;
        for (raw, &milli) in &self.0 {
            let code = normalize_currency(raw, overrides);
            match rates.get(&code).copied() {
                Some(1.0) => exact += i128::from(milli),
                Some(rate) if rate.is_finite() => converted += milli as f64 * rate,
                _ => {}
            }
        }
        // Keep unit-rate totals exact and round the combined result away from zero at ties.
        let whole = exact.saturating_add(converted.trunc() as i128);
        let fraction = converted.fract();
        let adjustment = if fraction > 0.5 || (fraction == 0.5 && whole >= 0) {
            1
        } else if fraction < -0.5 || (fraction == -0.5 && whole <= 0) {
            -1
        } else {
            0
        };
        clamp_milli(whole.saturating_add(adjustment))
    }

    pub fn only_milli(&self, code: &str, overrides: &BTreeMap<String, String>) -> i64 {
        let code = canonical_code(code.trim());
        let total = self
            .0
            .iter()
            .filter(|(raw, _)| normalize_currency(raw, overrides) == code)
            .map(|(_, &milli)| i128::from(milli))
            .sum();
        clamp_milli(total)
    }
}

fn clamp_milli(total: i128) -> i64 {
    total.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_amount_counts_mixed_unicode_decimal_digits() {
        let parsed = parse_display_amount("￥1２٣").unwrap();
        assert_eq!(parsed.currency, "￥");
        assert_eq!(parsed.milli, 123_000);
        assert_eq!(parse_display_amount("￥１．５").unwrap().milli, 1_500);
        assert_eq!(parse_display_amount("١٬٢٣٤٫٥").unwrap().milli, 1_234_500);
    }

    #[test]
    fn currency_evaluation_keeps_large_milli_totals_exact() {
        let milli = (1i64 << 53) + 1;
        let totals = AmountTotals(BTreeMap::from([("¥".to_owned(), milli)]));
        let rates = BTreeMap::from([("JPY".to_owned(), 1.0)]);
        assert_eq!(totals.jpy_milli(&rates, &BTreeMap::new()), milli);

        let totals = AmountTotals(BTreeMap::from([
            ("JPY".to_owned(), i64::MAX),
            ("JP¥".to_owned(), 1),
            ("¥".to_owned(), -1),
        ]));
        assert_eq!(totals.only_milli("JPY", &BTreeMap::new()), i64::MAX);
    }

    #[test]
    fn only_milli_accepts_lowercase_pseudo_currency() {
        let mut totals = AmountTotals::default();
        totals.add("pt", 1.0);
        assert_eq!(totals.only_milli("pt", &BTreeMap::new()), 1_000);
    }
}
