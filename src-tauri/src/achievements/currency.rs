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
    // A mark right before the first digit is a decimal point (`$.50`, `EUR,50`).
    // A mark followed by spacing stays in the label, as in `kr. 50`.
    let number_start = text[..first_digit]
        .strip_suffix(is_decimal_mark)
        .map_or(first_digit, str::len);
    let number_end = text.trim_end_matches(|c| !is_decimal_digit(c)).len();
    let currency = format!(
        "{}{}",
        text[..number_start].trim_matches(is_group_spacing),
        text[number_end..].trim_matches(is_group_spacing),
    );
    let number: String = text[number_start..number_end]
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

fn is_decimal_mark(c: char) -> bool {
    matches!(c, '.' | ',' | '．' | '，' | '٫')
}

fn is_group_spacing(c: char) -> bool {
    c.is_whitespace() || matches!(c, '٬' | '\'' | '’')
}

fn decimal_position(number: &str) -> Option<usize> {
    // A leading mark has no integer part to group, so it is always a decimal point (`,125`).
    if number.starts_with(['.', ',']) {
        return Some(0);
    }
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

/// 通貨表記・コードの前後を落とす。Unicode の White_Space(U+0085・U+3000 などを含む)と BOM(U+FEFF)を対象にする。
/// UI は値を変形せずに送るので、どの経路の trim もこれに揃える。
pub fn trim_currency(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// 受信した通貨表記を通貨コードにする。上書きは trim した表記の完全一致を優先し、無ければ ASCII の大小文字を無視して
/// 引く(保存時に大文字化するが、以前の版や手編集の小文字のキーにも効かせる)。一致した候補の行き先が食い違う時、
/// 表記が ISO 形式(英字 3 字)の時、行き先が ISO 形式でない時は上書きを使わない。上書きの行き先は常に ISO 形式で、
/// 候補が食い違う時は既知の記号(`US$` など)だけ標準のコードにし、それ以外は表記をそのまま返すので、2 回かけても
/// 結果は変わらない(保存し直してもレートのキーが動かない)。
pub fn normalize_currency(raw: &str, overrides: &BTreeMap<String, String>) -> String {
    let raw = trim_currency(raw);
    if !is_iso_code(raw) {
        match override_for(raw, overrides) {
            Override::Code(code) => return code,
            Override::Conflict => {
                return known_symbol(raw).map_or_else(|| raw.to_owned(), str::to_owned);
            }
            Override::None => {}
        }
    }
    known_symbol(raw).map_or_else(|| canonical_code(raw), str::to_owned)
}

fn known_symbol(raw: &str) -> Option<&'static str> {
    Some(match raw {
        "¥" | "￥" | "JP¥" => "JPY",
        "$" | "US$" => "USD",
        "CA$" => "CAD",
        "A$" => "AUD",
        "€" => "EUR",
        "£" => "GBP",
        "₩" => "KRW",
        "NT$" => "TWD",
        "HK$" => "HKD",
        "₹" => "INR",
        _ => return None,
    })
}

enum Override {
    None,
    Code(String),
    Conflict,
}

fn override_for(raw: &str, overrides: &BTreeMap<String, String>) -> Override {
    let pick = |matches: &dyn Fn(&str) -> bool| {
        let mut codes = overrides
            .iter()
            .filter(|(symbol, _)| matches(trim_currency(symbol)))
            .map(|(_, code)| trim_currency(code).to_ascii_uppercase());
        let Some(first) = codes.next() else { return Override::None };
        if codes.any(|code| code != first) {
            Override::Conflict
        } else if is_iso_code(&first) {
            Override::Code(first)
        } else {
            Override::None
        }
    };
    match pick(&|symbol| symbol == raw) {
        Override::None => pick(&|symbol| symbol.eq_ignore_ascii_case(raw)),
        found => found,
    }
}

/// 英字 3 字(大文字・小文字を問わない)。
pub fn is_iso_code(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

fn canonical_code(raw: &str) -> String {
    if is_iso_code(raw)
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
        let code = canonical_code(trim_currency(code));
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
    fn display_amount_preserves_currency_punctuation() {
        for input in ["kr. 50", "50 kr."] {
            let parsed = parse_display_amount(input).unwrap();
            assert_eq!(parsed.currency, "kr.");
            assert_eq!(parsed.milli, 50_000);
        }
        for (input, currency) in [("kr.50", "kr"), ("EUR,50", "EUR"), ("KWD٫٥٠", "KWD"), ("EUR.50", "EUR")] {
            let parsed = parse_display_amount(input).unwrap();
            assert_eq!(parsed.currency, currency, "{input}");
            assert_eq!(parsed.milli, 500, "{input}");
        }
        for input in ["EUR,125", "EUR，１２５", "$.125"] {
            let parsed = parse_display_amount(input).unwrap();
            assert_eq!(parsed.milli, 125, "{input}");
        }
        let parsed = parse_display_amount("$.50").unwrap();
        assert_eq!(parsed.currency, "$");
        assert_eq!(parsed.milli, 500);
    }

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

    fn ov(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn normalize_currency_is_idempotent_for_every_symbol_and_override() {
        let raws = [
            "pt", "PT", " pt ", "R$", "r$", "￥", "¥", "$", "brl", "BRL", "ｂｒｌ", "ＵＳＤ", "Ｐｔ", "__proto__",
            "__PROTO__", "zł", "bits", "X1", "Y2", "", "Pt", "pT", "uſd", "ſ", "s", "S", "\u{85}brl\u{85}",
            "\u{FEFF}pt", "\u{FEFF}R$\u{85}", "\u{FEFF}", "US$", "us$", "Us$",
        ];
        let tables = [
            ov(&[]),
            ov(&[("PT", "USD")]),
            ov(&[("R$", "BRL")]),
            ov(&[("￥", "CNY")]),
            ov(&[("$", "MXN")]),
            ov(&[("BRL", "USD")]),
            ov(&[("__PROTO__", "EUR")]),
            ov(&[("Ｐｔ", "KRW")]),
            ov(&[("X1", "Y2"), ("Y2", "Z3")]),
            ov(&[("R$", "BRL"), ("r$", "USD")]),
            ov(&[("PT", "USD"), ("pt", "EUR")]),
            ov(&[("us$", "EUR"), ("Us$", "CAD")]),
            ov(&[("ſ", "USD"), ("S", "EUR")]),
            ov(&[("R$", "BRL"), (" R$ ", "USD")]),
            ov(&[("PT", "USD"), ("USD", "EUR"), ("R$", "BRL"), ("BRL", "JPY"), ("BITS", "USD")]),
        ];
        for overrides in &tables {
            for raw in raws {
                let once = normalize_currency(raw, overrides);
                assert_eq!(normalize_currency(&once, overrides), once, "{raw:?} {overrides:?}");
            }
        }
    }

    #[test]
    fn overrides_match_case_insensitively_and_never_rewrite_iso_codes() {
        let pt = ov(&[("PT", "USD")]);
        for raw in ["pt", "PT", " Pt "] {
            assert_eq!(normalize_currency(raw, &pt), "USD", "{raw}");
        }
        assert_eq!(normalize_currency("pt", &ov(&[("pt", "usd")])), "USD");
        assert_eq!(normalize_currency("r$", &ov(&[("R$", "BRL")])), "BRL");
        let iso = ov(&[("BRL", "USD"), ("usd", "EUR")]);
        assert_eq!(normalize_currency("brl", &iso), "BRL");
        assert_eq!(normalize_currency(" BRL ", &iso), "BRL");
        assert_eq!(normalize_currency("usd", &iso), "USD");
        assert_eq!(normalize_currency("$", &ov(&[("$", "MXN")])), "MXN");
    }

    #[test]
    fn exact_override_wins_and_conflicting_case_variants_apply_none() {
        let legacy = ov(&[("R$", "BRL"), ("r$", "USD")]);
        assert_eq!(normalize_currency("r$", &legacy), "USD");
        assert_eq!(normalize_currency(" R$ ", &legacy), "BRL");
        let mut totals = AmountTotals::default();
        totals.add("r$", 2.0);
        totals.add("R$", 3.0);
        assert_eq!(totals.only_milli("BRL", &legacy), 3_000);
        assert_eq!(totals.only_milli("USD", &legacy), 2_000);
        // 完全一致が無く、大小文字を無視した候補の行き先が食い違う時は上書きしない。
        let conflict = ov(&[("PT", "USD"), ("pt", "EUR")]);
        assert_eq!(normalize_currency("pt", &conflict), "EUR");
        let pt = normalize_currency("Pt", &conflict);
        assert!(pt != "USD" && pt != "EUR", "{pt}");
        assert_eq!(normalize_currency("r$", &ov(&[("R$", "BRL"), ("R$\u{85}", "BRL")])), "BRL");
        // ſ は ASCII ではないので s / S の上書きに当たらない。
        assert_eq!(normalize_currency("ſ", &ov(&[("S", "EUR")])), "ſ");
        // 候補が食い違っても、既知の記号は標準のコードとして数える(US$ の投げ銭が USD 限定の実績から漏れない)。
        let us = ov(&[("us$", "EUR"), ("Us$", "CAD")]);
        assert_eq!(normalize_currency("US$", &us), "USD");
        let mut totals = AmountTotals::default();
        totals.add("US$", 1.0);
        assert_eq!(totals.only_milli("USD", &us), 1_000);
    }

    #[test]
    fn currency_trim_covers_unicode_white_space_and_bom() {
        assert_eq!(normalize_currency("\u{FEFF}pt\u{85}", &BTreeMap::new()), "PT");
        assert_eq!(normalize_currency("\u{3000}brl\u{2028}", &BTreeMap::new()), "BRL");
        assert_eq!(normalize_currency("\u{FEFF}R$", &ov(&[("R$\u{85}", "\u{FEFF}brl")])), "BRL");
        assert_eq!(normalize_currency("\u{200B}pt", &BTreeMap::new()), "\u{200B}pt");
    }

    #[test]
    fn only_milli_accepts_lowercase_pseudo_currency() {
        let mut totals = AmountTotals::default();
        totals.add("pt", 1.0);
        assert_eq!(totals.only_milli("pt", &BTreeMap::new()), 1_000);
    }
}
