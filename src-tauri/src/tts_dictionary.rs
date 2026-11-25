use std::collections::HashSet;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{TtsDictEntry, MAX_TTS_DICTIONARY_ENTRIES, TTS_DICTIONARY_ENTRY_LIMIT_ERROR};

mod download;
pub(crate) use download::fetch;

const SCHEMA: &str = "fast-comment-tts-dictionary";
const VERSION: u64 = 1;
const MAX_CHARS: usize = 200;
const MAX_REJECTIONS: usize = 20;
const REGEX_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ImportMode {
    Append,
    Replace,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportResult {
    pub added: usize,
    pub replaced: usize,
    pub skipped_duplicate: usize,
    pub rejected: Vec<Rejection>,
    pub rejected_count: usize,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct Rejection {
    /// JSON の entries 内の位置(1 始まり)。
    pub index: usize,
    pub reason: &'static str,
}

pub(crate) struct ParsedDictionary {
    entries: Vec<TtsDictEntry>,
    rejected: Vec<Rejection>,
    rejected_count: usize,
    explicitly_empty: bool,
}

pub(crate) fn export(entries: &[TtsDictEntry]) -> Result<String, String> {
    #[derive(Serialize)]
    struct Bundle<'a> {
        schema: &'static str,
        version: u64,
        entries: &'a [TtsDictEntry],
    }

    serde_json::to_string_pretty(&Bundle {
        schema: SCHEMA,
        version: VERSION,
        entries,
    })
    .map_err(|_| "読み上げ辞書を書き出せませんでした".into())
}

pub(crate) fn parse(raw: &[u8]) -> Result<ParsedDictionary, String> {
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    let root: Value =
        serde_json::from_slice(raw).map_err(|_| "読み上げ辞書のJSONを読めませんでした")?;
    let rows = match &root {
        Value::Array(rows) => rows,
        Value::Object(bundle) => {
            if bundle.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
                return Err("読み上げ辞書のschemaが不正です".into());
            }
            if bundle.get("version").and_then(Value::as_u64) != Some(VERSION) {
                return Err("未対応の読み上げ辞書のversionです".into());
            }
            bundle
                .get("entries")
                .and_then(Value::as_array)
                .ok_or("entriesは配列で指定してください")?
        }
        _ => return Err("読み上げ辞書はオブジェクトまたは配列で指定してください".into()),
    };
    if rows.len() > MAX_TTS_DICTIONARY_ENTRIES {
        return Err(TTS_DICTIONARY_ENTRY_LIMIT_ERROR.into());
    }

    let mut entries = Vec::with_capacity(rows.len());
    let mut rejected = Vec::new();
    let mut rejected_count = 0;
    for (index, row) in rows.iter().enumerate() {
        match parse_entry(row) {
            Ok(entry) => entries.push(entry),
            Err(reason) => {
                rejected_count += 1;
                if rejected.len() < MAX_REJECTIONS {
                    rejected.push(Rejection {
                        index: index + 1,
                        reason,
                    });
                }
            }
        }
    }
    Ok(ParsedDictionary {
        entries,
        rejected,
        rejected_count,
        explicitly_empty: root.is_object() && rows.is_empty(),
    })
}

fn parse_entry(row: &Value) -> Result<TtsDictEntry, &'static str> {
    let row = row
        .as_object()
        .ok_or("項目はオブジェクトで指定してください")?;
    let field = |names: &[&str]| names.iter().find_map(|name| row.get(*name));
    let from = field(&["from", "pattern", "word", "key"])
        .and_then(Value::as_str)
        .ok_or("fromは文字列で指定してください")?;
    if from.trim().is_empty() {
        return Err("fromに空白以外の文字を入力してください");
    }
    if from.chars().take(MAX_CHARS + 1).count() > MAX_CHARS {
        return Err("fromは空白を含めて200文字以内で指定してください");
    }
    let to = field(&["to", "reading", "replace", "value"])
        .and_then(Value::as_str)
        .ok_or("toは文字列で指定してください")?;
    if to.chars().take(MAX_CHARS + 1).count() > MAX_CHARS {
        return Err("toは200文字以内で指定してください");
    }
    let regex = match field(&["regex", "isRegex"]) {
        None => false,
        Some(value) => value.as_bool().ok_or("regexは真偽値で指定してください")?,
    };
    if regex {
        compile_regex(from).map_err(|error| match error {
            regex::Error::CompiledTooBig(_) => "正規表現がサイズ上限を超えています",
            _ => "正規表現が不正です",
        })?;
    }
    Ok(TtsDictEntry {
        from: from.into(),
        to: to.into(),
        regex,
    })
}

pub(crate) fn compile_regex(from: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(from)
        .size_limit(REGEX_LIMIT)
        .dfa_size_limit(REGEX_LIMIT)
        .build()
}

pub(crate) fn merge(
    current: &[TtsDictEntry],
    parsed: ParsedDictionary,
    mode: ImportMode,
) -> Result<(Vec<TtsDictEntry>, ImportResult), String> {
    let ParsedDictionary {
        entries,
        rejected,
        rejected_count,
        explicitly_empty,
    } = parsed;
    if entries.is_empty() {
        if rejected_count > 0 {
            use std::fmt::Write as _;

            let mut error =
                format!("有効な読み上げ辞書の項目がありません（拒否 {rejected_count} 件）");
            for Rejection { index, reason } in &rejected {
                let _ = write!(error, "\n{index} 件目 — {reason}");
            }
            let remaining = rejected_count - rejected.len();
            if remaining > 0 {
                let _ = write!(error, "\nほか {remaining} 件");
            }
            return Err(error);
        }
        if mode == ImportMode::Replace && !explicitly_empty {
            return Err(
                "辞書を空にするには、書き出し形式の entries に空の配列を指定してください".into(),
            );
        }
    }
    let mut result = ImportResult {
        added: 0,
        replaced: 0,
        skipped_duplicate: 0,
        rejected,
        rejected_count,
        total: 0,
    };
    let dictionary = match mode {
        ImportMode::Append => {
            let mut seen: HashSet<_> = current
                .iter()
                .map(|entry| (entry.from.as_str(), entry.regex))
                .collect();
            let mut dictionary = current.to_vec();
            for entry in &entries {
                if seen.insert((&entry.from, entry.regex)) {
                    dictionary.push(entry.clone());
                    result.added += 1;
                } else {
                    result.skipped_duplicate += 1;
                }
            }
            dictionary
        }
        ImportMode::Replace => {
            result.replaced = entries.len();
            entries
        }
    };
    if dictionary.len() > MAX_TTS_DICTIONARY_ENTRIES {
        return Err(TTS_DICTIONARY_ENTRY_LIMIT_ERROR.into());
    }
    result.total = dictionary.len();
    Ok((dictionary, result))
}

#[cfg(test)]
mod tests;
