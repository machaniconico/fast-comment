// registered in main.rs by story ACH-001
//! 通貨の解析・正規化・合計(acceptance.md §3.2、D3・D13)。期待値は spec の表と手計算の定数。
use fast_comment_lib::achievements::currency::{
    amount_from_micros, normalize_currency, parse_display_amount, AmountTotals, RateTable,
};
use std::collections::BTreeMap;

fn none() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn ov(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn rates(pairs: &[(&str, f64)]) -> RateTable {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn parsed(text: &str) -> (String, i64) {
    let p = parse_display_amount(text).unwrap_or_else(|| panic!("{text:?} は解析できるはず"));
    (p.currency, p.milli)
}

#[test]
fn parse_display_amount_fixture_table() {
    let table: [(&str, &str, i64); 8] = [
        ("CHF 1’234.50", "CHF", 1_234_500),
        ("1\u{00A0}234,56\u{00A0}€", "€", 1_234_560),
        ("€1.234,56", "€", 1_234_560),
        ("￥1,000", "￥", 1_000_000),
        ("$5.00", "$", 5_000),
        ("CA$5", "CA$", 5_000),
        ("R$ 12,50", "R$", 12_500),
        ("¥1,234,567", "¥", 1_234_567_000),
    ];
    for (text, cur, milli) in table {
        assert_eq!(parsed(text), (cur.to_string(), milli), "入力 {text:?}");
    }
    assert!(parse_display_amount("—").is_none());
}

#[test]
fn parse_display_amount_strips_apostrophe_from_currency() {
    assert_eq!(parsed("CHF 1’234.50"), ("CHF".to_string(), 1_234_500));
    assert_eq!(parsed("CHF 1'234.50"), ("CHF".to_string(), 1_234_500));
}

#[test]
fn parse_display_amount_handles_nbsp_and_narrow_space() {
    assert_eq!(parsed("1\u{00A0}234,56\u{00A0}€"), ("€".to_string(), 1_234_560));
    assert_eq!(parsed("1\u{202F}234,56\u{202F}€"), ("€".to_string(), 1_234_560));
    assert_eq!(parsed("¥1\u{2009}000"), ("¥".to_string(), 1_000_000));
}

#[test]
fn parse_display_amount_returns_none_without_digits() {
    for t in ["—", "", "   ", "abc", "$"] {
        assert!(parse_display_amount(t).is_none(), "{t:?}");
    }
}

#[test]
fn amount_from_micros_uppercases_iso_and_rounds() {
    let a = amount_from_micros(1_234_500_000, "chf");
    assert_eq!((a.currency.as_str(), a.milli), ("CHF", 1_234_500));
    let b = amount_from_micros(5_000_000, "USD");
    assert_eq!((b.currency.as_str(), b.milli), ("USD", 5_000));
    let c = amount_from_micros(1_234_567, "usd");
    assert_eq!(c.milli, 1_235, "1234.567 は四捨五入で 1235");
    let d = amount_from_micros(1_234_400, "usd");
    assert_eq!(d.milli, 1_234, "1234.4 は 1234");
}

#[test]
fn normalize_maps_yen_and_dollar_symbols_to_iso() {
    let cases = [
        ("¥", "JPY"),
        ("￥", "JPY"),
        ("JP¥", "JPY"),
        ("$", "USD"),
        ("US$", "USD"),
        ("€", "EUR"),
        ("£", "GBP"),
        ("₩", "KRW"),
        ("₹", "INR"),
    ];
    for (raw, iso) in cases {
        assert_eq!(normalize_currency(raw, &none()), iso, "{raw}");
    }
}

#[test]
fn normalize_distinguishes_cad_aud_twd_hkd() {
    assert_eq!(normalize_currency("CA$", &none()), "CAD");
    assert_eq!(normalize_currency("A$", &none()), "AUD");
    assert_eq!(normalize_currency("NT$", &none()), "TWD");
    assert_eq!(normalize_currency("HK$", &none()), "HKD");
}

#[test]
fn normalize_override_wins_over_builtin_table() {
    assert_eq!(normalize_currency("¥", &ov(&[("¥", "CNY")])), "CNY");
    assert_eq!(normalize_currency("$", &ov(&[("$", "MXN")])), "MXN");
    assert_eq!(normalize_currency("R$", &ov(&[("R$", "BRL")])), "BRL");
}

#[test]
fn normalize_uppercases_iso_and_pseudo_codes() {
    assert_eq!(normalize_currency("usd", &none()), "USD");
    assert_eq!(normalize_currency("eur", &none()), "EUR");
    assert_eq!(normalize_currency("pt", &none()), "PT");
    assert_eq!(normalize_currency("BITS", &none()), "BITS");
}

#[test]
fn normalize_unknown_returns_trimmed_raw() {
    assert_eq!(normalize_currency("R$", &none()), "R$");
    assert_eq!(normalize_currency("  zł  ", &none()), "zł");
}

#[test]
fn unknown_symbol_never_matches_only_iso_unless_mapped() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("R$", 12.5);
    assert_eq!(t.only_milli("BRL", &none()), 0);
    assert_eq!(t.only_milli("BRL", &ov(&[("R$", "BRL")])), 12_500);
}

#[test]
fn totals_keep_raw_symbol_keys_separate() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("¥", 100.0);
    t.add("JPY", 200.0);
    t.add("$", 1.0);
    t.add("¥", 50.0);
    assert_eq!(t.0.len(), 3);
    assert_eq!(t.0.get("¥"), Some(&150_000));
    assert_eq!(t.0.get("JPY"), Some(&200_000));
    assert_eq!(t.0.get("$"), Some(&1_000));
}

#[test]
fn totals_store_milli_units_without_float_drift() {
    let mut t = AmountTotals(BTreeMap::new());
    for _ in 0..10 {
        t.add("USD", 0.1);
    }
    assert_eq!(t.0.get("USD"), Some(&1_000));
}

#[test]
fn jpy_uses_rate_and_excludes_unset_codes() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("¥", 100.0);
    t.add("JPY", 50.0);
    t.add("$", 2.0);
    t.add("R$", 10.0);
    let r = rates(&[("JPY", 1.0), ("USD", 150.0)]);
    // (100 + 50) 円 + 2 USD × 150 円 = 450 円。R$ はレートなしで 0。
    assert_eq!(t.jpy_milli(&r, &none()), 450_000);
}

#[test]
fn jpy_excludes_bits_and_pt_without_rate() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("BITS", 100.0);
    t.add("PT", 100.0);
    t.add("JPY", 1.0);
    assert_eq!(t.jpy_milli(&rates(&[("JPY", 1.0)]), &none()), 1_000);
}

#[test]
fn jpy_includes_bits_when_rate_set() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("BITS", 100.0);
    assert_eq!(t.jpy_milli(&rates(&[("JPY", 1.0), ("BITS", 1.4)]), &none()), 140_000);
}

#[test]
fn jpy_recomputes_after_symbol_override_change() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("R$", 10.0);
    let r = rates(&[("JPY", 1.0), ("BRL", 30.0)]);
    assert_eq!(t.jpy_milli(&r, &none()), 0);
    assert_eq!(t.jpy_milli(&r, &ov(&[("R$", "BRL")])), 300_000);
}

#[test]
fn only_sums_matching_normalized_code() {
    let mut t = AmountTotals(BTreeMap::new());
    t.add("¥", 100.0);
    t.add("JPY", 50.0);
    t.add("$", 3.0);
    assert_eq!(t.only_milli("JPY", &none()), 150_000);
    assert_eq!(t.only_milli("USD", &none()), 3_000);
    assert_eq!(t.only_milli("EUR", &none()), 0);
}
