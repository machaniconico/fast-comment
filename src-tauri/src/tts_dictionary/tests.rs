use super::*;
use crate::config::TtsOptions;
use serde_json::json;

fn row(from: &str, to: &str, regex: bool) -> TtsDictEntry {
    TtsDictEntry {
        from: from.into(),
        to: to.into(),
        regex,
    }
}

fn parse_json(value: Value) -> ParsedDictionary {
    parse(&serde_json::to_vec(&value).unwrap()).unwrap()
}

#[test]
fn bundle_round_trips_and_ignores_unknown_fields() {
    let expected = vec![row("配信", "はいしん", false), row("(草)+", "$1", true)];
    let exported = export(&expected).unwrap();
    let mut bundle: Value = serde_json::from_str(&exported).unwrap();
    assert_eq!(
        bundle,
        json!({
            "schema": SCHEMA,
            "version": 1,
            "entries": expected,
        })
    );
    bundle["extra"] = json!({"anything": ["ignored"]});
    bundle["entries"][0]["label"] = json!("extra");
    let parsed = parse_json(bundle.clone());
    assert_eq!(parsed.entries, expected);
    assert!(parsed.rejected.is_empty());

    bundle["version"] = json!(2);
    let unsupported = parse(&serde_json::to_vec(&bundle).unwrap()).err().unwrap();
    assert!(unsupported.contains("version"));
    bundle["version"] = json!(1);
    bundle["schema"] = json!("other");
    assert!(parse(&serde_json::to_vec(&bundle).unwrap()).is_err());
}

#[test]
fn accepts_array_and_all_aliases() {
    let parsed = parse_json(json!([
        {"pattern": "  w+  ", "reading": "わら", "isRegex": true},
        {"word": "配信", "replace": "はいしん"},
        {"key": "草", "value": "", "isRegex": false},
    ]));
    assert_eq!(
        parsed.entries,
        [
            row("w+", "わら", true),
            row("配信", "はいしん", false),
            row("草", "", false)
        ]
    );
    assert!(parsed.rejected.is_empty());
}

#[test]
fn accepts_crlf_json_and_preserves_replacement_whitespace() {
    let raw = "[\r\n  {\"from\":\"草\",\"to\":\" くさ\\r\\n \"}\r\n]";
    assert_eq!(
        parse(raw.as_bytes()).unwrap().entries,
        [row("草", " くさ\r\n ", false)]
    );
    assert!(parse(b"not JSON").is_err());
}

#[test]
fn limits_input_to_1000_rows_before_validating_rows() {
    let mut rows = vec![json!({"from": "草", "to": "くさ"}); 1000];
    assert_eq!(parse_json(json!(rows)).entries.len(), 1000);
    rows.push(Value::Null);
    assert_eq!(
        parse(&serde_json::to_vec(&rows).unwrap()).err().as_deref(),
        Some(ENTRY_LIMIT_ERROR)
    );
}

#[test]
fn lengths_count_full_width_and_emoji_characters() {
    let full_width = "全".repeat(200);
    let emoji = "🍣".repeat(200);
    let parsed = parse_json(json!([
        {"from": format!("　{full_width} "), "to": emoji},
        {"from": format!("{full_width}全"), "to": ""},
        {"from": "草", "to": format!("{emoji}🍣")},
        {"from": "　 ", "to": ""},
        {"from": emoji, "to": full_width},
        {"from": "🍣", "to": ""}
    ]));
    assert_eq!(
        parsed.entries,
        [
            row(&full_width, &emoji, false),
            row(&emoji, &full_width, false),
            row("🍣", "", false)
        ]
    );
    assert_eq!(
        parsed.rejected.iter().map(|r| r.index).collect::<Vec<_>>(),
        [2, 3, 4]
    );
}

#[test]
fn invalid_and_oversized_regexes_only_reject_their_own_rows() {
    let huge = "[A-Za-z]{900000}";
    assert!(matches!(
        compile_regex(huge),
        Err(regex::Error::CompiledTooBig(_))
    ));
    let parsed = parse_json(json!([
        {"from": "(", "to": "かっこ", "regex": true},
        {"from": huge, "to": "", "regex": true},
        {"from": "(", "to": "かっこ", "regex": false},
        {"from": "w+", "to": "わら", "regex": true}
    ]));
    assert_eq!(
        parsed.entries,
        [row("(", "かっこ", false), row("w+", "わら", true)]
    );
    assert_eq!(parsed.rejected[0].index, 1);
    assert_eq!(parsed.rejected[0].reason, "正規表現が不正です");
    assert_eq!(parsed.rejected[1].index, 2);
    assert_eq!(
        parsed.rejected[1].reason,
        "正規表現がサイズ上限を超えています"
    );
}

#[test]
fn returns_at_most_first_20_rejection_details() {
    let mut rows = vec![Value::Null; 21];
    rows.push(json!({"from": "草", "to": "くさ"}));
    let (dictionary, result) = merge(&[], parse_json(json!(rows)), ImportMode::Append).unwrap();
    assert_eq!(dictionary, [row("草", "くさ", false)]);
    assert_eq!(result.rejected.len(), 20);
    assert_eq!(result.rejected[19].index, 20);
    assert_eq!(result.total, 1);
}

#[test]
fn append_keeps_order_and_skips_duplicate_keys() {
    let current = [row("草", "くさ", false), row("配信", "はいしん", false)];
    let parsed = parse_json(json!([
        {"from": "草", "to": "既存を優先"},
        {"from": "草", "to": "わら", "regex": true},
        {"from": " 草 ", "to": "同じキー", "regex": true},
        {"from": "枠", "to": "わく"},
    ]));
    let (dictionary, result) = merge(&current, parsed, ImportMode::Append).unwrap();
    assert_eq!(
        dictionary,
        [
            current[0].clone(),
            current[1].clone(),
            row("草", "わら", true),
            row("枠", "わく", false)
        ]
    );
    assert_eq!(
        (
            result.added,
            result.replaced,
            result.skipped_duplicate,
            result.total
        ),
        (2, 0, 2, 4)
    );
}

#[test]
fn replace_removes_previous_entries_and_keeps_import_order() {
    let current = [row("旧", "きゅう", false)];
    let parsed = parse_json(json!([
        {"from": "草", "to": "くさ"},
        {"from": "草", "to": "わら"},
    ]));
    let (dictionary, result) = merge(&current, parsed, ImportMode::Replace).unwrap();
    assert_eq!(
        dictionary,
        [row("草", "くさ", false), row("草", "わら", false)]
    );
    assert_eq!(
        (
            result.added,
            result.replaced,
            result.skipped_duplicate,
            result.total
        ),
        (0, 2, 0, 2)
    );
}

#[test]
fn limits_combined_dictionary_after_duplicate_filtering() {
    let current = (0..999)
        .map(|i| row(&i.to_string(), "", false))
        .collect::<Vec<_>>();
    let valid = parse_json(json!([{"from": "999", "to": ""}, {"from": "0", "to": "重複"}]));
    let (full, result) = merge(&current, valid, ImportMode::Append).unwrap();
    assert_eq!(result.total, 1000);
    assert_eq!(result.skipped_duplicate, 1);
    let overflow = parse_json(json!([{"from": "1000", "to": ""}]));
    assert_eq!(
        merge(&full, overflow, ImportMode::Append).unwrap_err(),
        ENTRY_LIMIT_ERROR
    );
}

#[test]
fn config_normalize_keeps_first_1000_entries() {
    let mut options = TtsOptions {
        dictionary: (0..=1000).map(|i| row(&i.to_string(), "", false)).collect(),
        ..Default::default()
    };
    let expected = options.dictionary[..1000].to_vec();
    options.normalize();
    assert_eq!(options.dictionary, expected);
    options.normalize();
    assert_eq!(options.dictionary, expected);
}
