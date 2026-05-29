//! YouTube Live Chat レスポンスの寛容パーサ(アダプタの核)。
//!
//! 方針(SPEC §4.2):
//! - 固い struct deserialize はしない。`serde_json::Value` をパス探索で辿る。
//! - ヘルパ `dig` で Option を返し、途中欠落でも None で安全に劣化させる。
//! - 解析不能な `addChatItemAction` は `logs/yt-unparsed.jsonl` に1行追記。
//! - パーサにバージョンタグを持たせ、将来の差し替えを容易にする。

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use crate::model::{Amount, Author, Badge, ChatMessage, Fragment, MessageKind, Platform, Roles};

/// パーサのバージョン。レスポンス構造の解釈が変わったら上げる。
pub const PARSER_VERSION: &str = "yt-1";

static UNPARSED_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

// ─────────────────────────────────────────────────────────────────────────
// youtube_overrides.paths のキー名(SPEC §4.2「抽出パスを再ビルド無しで差し替え」)。
// いずれも欠落/空のときは下記ハードコード既定にフォールバックし、現行挙動を維持する。
// ─────────────────────────────────────────────────────────────────────────

/// actions 配列までの探索パス(`>` 区切りのキー列)。
/// 既定 `continuationContents>liveChatContinuation>actions`。
const KEY_ACTIONS_PATH: &str = "actionsPath";
/// continuations 配列までの探索パス(`>` 区切りのキー列)。
/// 既定 `continuationContents>liveChatContinuation>continuations`。
const KEY_CONTINUATIONS_PATH: &str = "continuationsPath";
/// continuations[] の入れ子 continuationData キー群(改行区切りで複数指定可)。
/// 既定は下記 `DEFAULT_CONTINUATION_DATA_KEYS`。
const KEY_CONTINUATION_DATA_KEYS: &str = "continuationDataKeys";

const DEFAULT_ACTIONS_PATH: &[&str] =
    &["continuationContents", "liveChatContinuation", "actions"];
const DEFAULT_CONTINUATIONS_PATH: &[&str] =
    &["continuationContents", "liveChatContinuation", "continuations"];
const DEFAULT_CONTINUATION_DATA_KEYS: &[&str] = &[
    "invalidationContinuationData",
    "timedContinuationData",
    "reloadContinuationData",
    "liveChatReplayContinuationData",
    "playerSeekContinuationData",
];

/// `paths` のキー値(`>` 区切り)をキー列へ分割。欠落/空なら `default` を返す。
fn split_path<'a>(
    paths: &'a HashMap<String, String>,
    key: &str,
    default: &'static [&'static str],
) -> Vec<&'a str> {
    match paths.get(key).map(String::as_str).filter(|s| !s.is_empty()) {
        Some(s) => s.split('>').map(str::trim).filter(|p| !p.is_empty()).collect(),
        None => default.to_vec(),
    }
}

/// `paths` のキー値(改行区切り)をキー群へ分割。欠落/空なら `default` を返す。
fn split_lines<'a>(
    paths: &'a HashMap<String, String>,
    key: &str,
    default: &'static [&'static str],
) -> Vec<&'a str> {
    match paths.get(key).map(String::as_str).filter(|s| !s.is_empty()) {
        Some(s) => s.split('\n').map(str::trim).filter(|p| !p.is_empty()).collect(),
        None => default.to_vec(),
    }
}

/// `Value` をパスで辿るヘルパ。文字列キーと配列インデックスを混在指定できる。
///
/// 途中で型が合わない/キーが無ければ `None`。
///
/// 例: `dig(&v, &[Key("a"), Idx(0), Key("b")])`
pub enum Seg<'a> {
    Key(&'a str),
    Idx(usize),
}

/// パス探索本体。
pub fn dig<'a>(value: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    let mut cur = value;
    for seg in path {
        cur = match seg {
            Seg::Key(k) => cur.get(*k)?,
            Seg::Idx(i) => cur.get(*i)?,
        };
    }
    Some(cur)
}

/// 文字列キーのみの簡易パス探索(よく使う形)。
fn dig_keys<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut cur = value;
    for k in keys {
        cur = cur.get(*k)?;
    }
    Some(cur)
}

/// レスポンスから actions 配列を取り出す。
///
/// 通常は `continuationContents.liveChatContinuation.actions`。
/// 探索パスは `paths` の `actionsPath`(`>` 区切り)で差し替え可能。
/// 欠落/空のときは既定パスで現行どおり辿る。
pub fn extract_actions(resp: &Value, paths: &HashMap<String, String>) -> Vec<Value> {
    let keys = split_path(paths, KEY_ACTIONS_PATH, DEFAULT_ACTIONS_PATH);
    dig_keys(resp, &keys)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// 次の continuation トークンと timeoutMs を取り出す。
///
/// `continuations[0]` 配下の各種 continuationData を寛容に探す。
/// continuations への探索パスは `paths` の `continuationsPath`(`>` 区切り)、
/// 入れ子の continuationData キー群は `continuationDataKeys`(改行区切り)で
/// 差し替え可能。欠落/空のときは既定で現行どおり辿る。
pub fn next_continuation(resp: &Value, paths: &HashMap<String, String>) -> (Option<String>, Option<u64>) {
    let conts_path = split_path(paths, KEY_CONTINUATIONS_PATH, DEFAULT_CONTINUATIONS_PATH);
    let conts = dig_keys(resp, &conts_path).and_then(|v| v.as_array());

    let Some(conts) = conts else {
        return (None, None);
    };

    let data_keys = split_lines(paths, KEY_CONTINUATION_DATA_KEYS, DEFAULT_CONTINUATION_DATA_KEYS);

    for cont in conts {
        // 入れ子のキー名は複数パターンある。順に試す。
        for key in &data_keys {
            if let Some(data) = cont.get(*key) {
                let token = data
                    .get("continuation")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let timeout = data
                    .get("timeoutMs")
                    .and_then(|v| v.as_u64());
                if token.is_some() {
                    return (token, timeout);
                }
            }
        }
    }

    (None, None)
}

/// このアクションが `addChatItemAction`(本来パースすべき種類)かどうか。
/// 解析失敗時のログ対象を絞り込むのに使う。
pub fn is_chat_item_action(action: &Value) -> bool {
    action.get("addChatItemAction").is_some()
}

/// 1アクションを `ChatMessage` に正規化する。対応外なら `None`。
///
/// 対応レンダラ:
/// - liveChatTextMessageRenderer  : 通常コメント
/// - liveChatPaidMessageRenderer  : SuperChat
/// - liveChatMembershipItemRenderer: メンバーシップ
/// - liveChatPaidStickerRenderer  : SuperSticker
pub fn parse_action(action: &Value, channel: &str) -> Option<ChatMessage> {
    // addChatItemAction.item の中に各種 renderer がぶら下がる。
    let item = dig_keys(action, &["addChatItemAction", "item"])?;

    if let Some(r) = item.get("liveChatTextMessageRenderer") {
        return parse_text_message(r, channel);
    }
    if let Some(r) = item.get("liveChatPaidMessageRenderer") {
        return parse_paid_message(r, channel);
    }
    if let Some(r) = item.get("liveChatMembershipItemRenderer") {
        return parse_membership(r, channel);
    }
    if let Some(r) = item.get("liveChatPaidStickerRenderer") {
        return parse_paid_sticker(r, channel);
    }

    None
}

/// 通常テキストコメント。
fn parse_text_message(r: &Value, channel: &str) -> Option<ChatMessage> {
    let author = parse_author(r);
    let fragments = parse_runs(r.get("message"));
    Some(build_message(
        r,
        channel,
        author,
        fragments,
        MessageKind::Normal,
        None,
    ))
}

/// SuperChat。金額と本文(本文は無いこともある)。
fn parse_paid_message(r: &Value, channel: &str) -> Option<ChatMessage> {
    let author = parse_author(r);
    let mut fragments = parse_runs(r.get("message"));
    // SuperChat は本文無しもある。空なら金額表記を本文代わりに入れる。
    let amount = parse_amount(r.get("purchaseAmountText"));
    if fragments.is_empty() {
        if let Some(a) = &amount {
            fragments.push(Fragment::text(a.raw_text.clone()));
        }
    }
    Some(build_message(
        r,
        channel,
        author,
        fragments,
        MessageKind::SuperChat,
        amount,
    ))
}

/// SuperSticker。金額あり、本文はステッカー説明。
fn parse_paid_sticker(r: &Value, channel: &str) -> Option<ChatMessage> {
    let author = parse_author(r);
    let amount = parse_amount(r.get("purchaseAmountText"));
    // ステッカーには message runs が無い。accessibility ラベルかステッカー名を本文に。
    let label = dig_keys(
        r,
        &["sticker", "accessibility", "accessibilityData", "label"],
    )
    .and_then(|v| v.as_str())
    .map(|s| s.to_string())
    .unwrap_or_else(|| "[SuperSticker]".to_string());

    let fragments = vec![Fragment::text(label)];
    Some(build_message(
        r,
        channel,
        author,
        fragments,
        MessageKind::SuperChat,
        amount,
    ))
}

/// メンバーシップ(新規加入/継続/ギフト)。
fn parse_membership(r: &Value, channel: &str) -> Option<ChatMessage> {
    let author = parse_author(r);
    // 加入時は headerSubtext、メッセージ付き継続は message に入る。両方試す。
    let mut fragments = parse_runs(r.get("message"));
    if fragments.is_empty() {
        fragments = parse_runs(r.get("headerSubtext"));
    }
    if fragments.is_empty() {
        fragments = parse_runs(r.get("headerPrimaryText"));
    }
    if fragments.is_empty() {
        fragments.push(Fragment::text("[Membership]".to_string()));
    }
    Some(build_message(
        r,
        channel,
        author,
        fragments,
        MessageKind::Membership,
        None,
    ))
}

/// レンダラ共通のフィールドから `ChatMessage` を組む。
fn build_message(
    r: &Value,
    channel: &str,
    author: Author,
    fragments: Vec<Fragment>,
    kind: MessageKind,
    amount: Option<Amount>,
) -> ChatMessage {
    let id = r
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(ChatMessage::new_id);

    // timestampUsec はマイクロ秒文字列。ms に変換。
    let timestamp_ms = r
        .get("timestampUsec")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .map(|usec| usec / 1000)
        .unwrap_or_else(now_ms);

    ChatMessage {
        id,
        platform: Platform::Youtube,
        channel: channel.to_string(),
        author,
        fragments,
        kind,
        amount,
        timestamp_ms,
        raw: None,
    }
}

/// 著者情報(名前/ID/色/バッジ/ロール)をレンダラから抽出。
fn parse_author(r: &Value) -> Author {
    let name = r
        .get("authorName")
        .and_then(|v| simple_text(v))
        .unwrap_or_default();

    let id = r
        .get("authorExternalChannelId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let (roles, badges) = parse_author_badges(r.get("authorBadges"));

    Author {
        id,
        name,
        display_color: None, // YouTube はユーザー色を持たない(バッジ色は別途)。
        badges,
        roles,
    }
}

/// `authorBadges[]` から Roles と Badge を作る。
///
/// バッジには `liveChatAuthorBadgeRenderer` があり、`icon.iconType`(OWNER/MODERATOR/VERIFIED)
/// もしくは `customThumbnail`(メンバーバッジ)を持つ。
fn parse_author_badges(badges: Option<&Value>) -> (Roles, Vec<Badge>) {
    let mut roles = Roles::default();
    let mut out = Vec::new();

    let Some(arr) = badges.and_then(|v| v.as_array()) else {
        return (roles, out);
    };

    for b in arr {
        let Some(br) = b.get("liveChatAuthorBadgeRenderer") else {
            continue;
        };

        // ラベル(tooltip)。
        let label = br
            .get("tooltip")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        // アイコン種別(OWNER/MODERATOR/VERIFIED 等)。
        let icon_type = dig_keys(br, &["icon", "iconType"])
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let kind = match icon_type {
            "OWNER" => {
                roles.broadcaster = true;
                "broadcaster".to_string()
            }
            "MODERATOR" => {
                roles.moderator = true;
                "moderator".to_string()
            }
            "VERIFIED" => "verified".to_string(),
            "" => {
                // customThumbnail があればメンバーバッジとみなす。
                if br.get("customThumbnail").is_some() {
                    roles.member = true;
                    "member".to_string()
                } else {
                    "badge".to_string()
                }
            }
            other => other.to_lowercase(),
        };

        // メンバーバッジ画像URL(あれば最初の thumbnail)。
        let image_url = dig(
            br,
            &[
                Seg::Key("customThumbnail"),
                Seg::Key("thumbnails"),
                Seg::Idx(0),
                Seg::Key("url"),
            ],
        )
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

        out.push(Badge {
            kind,
            label,
            image_url,
        });
    }

    (roles, out)
}

/// `message`/`headerSubtext` 等の `{ runs: [...] }` を Fragment 列に変換。
///
/// 各 run は `text`(テキスト)または `emoji`(エモート)を持つ。
fn parse_runs(field: Option<&Value>) -> Vec<Fragment> {
    let Some(field) = field else {
        return Vec::new();
    };

    // `{ simpleText: "..." }` 形式にも対応。
    if let Some(s) = field.get("simpleText").and_then(|v| v.as_str()) {
        if s.is_empty() {
            return Vec::new();
        }
        return vec![Fragment::text(s.to_string())];
    }

    let Some(runs) = field.get("runs").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut fragments = Vec::new();
    for run in runs {
        if let Some(text) = run.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                fragments.push(Fragment::text(text.to_string()));
            }
        } else if let Some(emoji) = run.get("emoji") {
            fragments.push(parse_emoji(emoji));
        }
    }
    fragments
}

/// `emoji` run を Emote Fragment に変換。
fn parse_emoji(emoji: &Value) -> Fragment {
    let id = emoji
        .get("emojiId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    // 表示名: shortcuts[0] か searchTerms[0]。無ければ id。
    let name = dig(emoji, &[Seg::Key("shortcuts"), Seg::Idx(0)])
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            dig(emoji, &[Seg::Key("searchTerms"), Seg::Idx(0)])
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| id.clone());

    // 画像URL: image.thumbnails[last].url。
    let url = emoji
        .get("image")
        .and_then(|img| img.get("thumbnails"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
        .and_then(|t| t.get("url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    // カスタム絵文字でない(unicode 絵文字)場合は emojiId が絵文字そのもの。
    // url が空ならテキスト断片として扱った方が自然だが、モデル契約に従い Emote で返す。
    if url.is_empty() {
        // unicode 絵文字: name(=絵文字文字)をそのままテキストに。
        return Fragment::text(name);
    }

    Fragment::Emote { id, name, url }
}

/// `purchaseAmountText`({ simpleText: "¥500" })から Amount を作る。
fn parse_amount(field: Option<&Value>) -> Option<Amount> {
    let raw = field.and_then(simple_text)?;
    if raw.is_empty() {
        return None;
    }
    let (currency, value) = split_currency_value(&raw);
    Some(Amount {
        value,
        currency,
        raw_text: raw,
    })
}

/// `{ simpleText }` または `{ runs:[{text}] }` から平文を取り出す。
fn simple_text(v: &Value) -> Option<String> {
    if let Some(s) = v.get("simpleText").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    if let Some(runs) = v.get("runs").and_then(|x| x.as_array()) {
        let s: String = runs
            .iter()
            .filter_map(|r| r.get("text").and_then(|t| t.as_str()))
            .collect();
        if !s.is_empty() {
            return Some(s);
        }
    }
    // 文字列リテラル直渡しも許容。
    v.as_str().map(|s| s.to_string())
}

/// "¥500" / "$5.00" / "￥1,000" 等から通貨記号と数値を分離する(寛容)。
///
/// 数値は内部的に小数点ドットへ正規化し、桁区切りカンマは除去する。
/// 欧州表記のように `.` と `,` が混在し `,` が後ろにある場合は、`,` を小数点と
/// みなして `.` を桁区切りとして扱う。
fn split_currency_value(raw: &str) -> (String, f64) {
    // 通貨部 = 数字/小数点/桁区切り/空白 以外の連続部分。
    let currency: String = raw
        .chars()
        .filter(|c| !c.is_ascii_digit() && *c != '.' && *c != ',' && !c.is_whitespace())
        .collect();

    let number: String = normalize_amount_number(
        &raw
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
            .collect::<String>(),
    );

    let value = number.parse::<f64>().unwrap_or(0.0);
    let currency = if currency.is_empty() {
        "".to_string()
    } else {
        currency
    };
    (currency, value)
}

fn normalize_amount_number(number: &str) -> String {
    let last_dot = number.rfind('.');
    let last_comma = number.rfind(',');

    if let (Some(dot), Some(comma)) = (last_dot, last_comma) {
        if comma > dot {
            return number
                .chars()
                .filter_map(|c| match c {
                    '.' => None,
                    ',' => Some('.'),
                    _ => Some(c),
                })
                .collect();
        }

        return number.chars().filter(|c| *c != ',').collect();
    }

    if last_comma.is_some() {
        return normalize_comma_only_number(number);
    }

    number.to_string()
}

fn normalize_comma_only_number(number: &str) -> String {
    let parts: Vec<&str> = number.split(',').collect();
    if parts.len() > 1
        && parts[0].len() <= 3
        && parts[1..].iter().all(|part| part.len() == 3)
    {
        return parts.concat();
    }

    if parts.len() == 2 && matches!(parts[1].len(), 1 | 2) {
        return format!("{}.{}", parts[0], parts[1]);
    }

    number
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect()
}

/// 解析できなかったアクションを `logs/yt-unparsed.jsonl` に1行追記する。
///
/// 失敗してもアプリ動作は止めない(best-effort)。
pub fn log_unparsed(action: &Value) {
    if let Err(e) = try_log_unparsed(action) {
        tracing::debug!("yt-unparsed ログ追記に失敗: {e}");
    }
}

fn try_log_unparsed(action: &Value) -> std::io::Result<()> {
    let dir = unparsed_log_dir();
    std::fs::create_dir_all(dir)?;
    let path = dir.join("yt-unparsed.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    // バージョンタグ付きで1行 JSON を書く。
    let line = serde_json::json!({
        "parserVersion": PARSER_VERSION,
        "ts": now_ms(),
        "action": action,
    });
    writeln!(f, "{}", line)?;
    Ok(())
}

fn unparsed_log_dir() -> &'static Path {
    UNPARSED_LOG_DIR
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|parent| parent.join("logs")))
                .unwrap_or_else(|| PathBuf::from("logs"))
        })
        .as_path()
}

/// 現在時刻(unix ms)。
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::{dig, extract_actions, parse_action, parse_runs, split_currency_value, Seg};
    use crate::model::{Fragment, MessageKind};

    #[test]
    fn dig_returns_value_or_none_for_missing_path() {
        // dig は存在するパスを返し、途中欠落や型違いは None に落とす。
        let payload = json!({
            "outer": [
                { "inner": { "leaf": 42 } }
            ]
        });

        let found = dig(
            &payload,
            &[
                Seg::Key("outer"),
                Seg::Idx(0),
                Seg::Key("inner"),
                Seg::Key("leaf"),
            ],
        )
        .and_then(|v| v.as_i64());

        assert_eq!(found, Some(42));
        assert!(dig(&payload, &[Seg::Key("outer"), Seg::Idx(1)]).is_none());
        assert!(
            dig(
                &payload,
                &[
                    Seg::Key("outer"),
                    Seg::Idx(0),
                    Seg::Key("missing"),
                    Seg::Key("leaf"),
                ],
            )
            .is_none()
        );
        assert!(dig(&payload, &[Seg::Key("outer"), Seg::Key("inner")]).is_none());
    }

    #[test]
    fn parse_runs_converts_text_and_emoji_fragments() {
        // runs[] の text は Text、画像 URL 付き emoji は Emote、URL 空の emoji は Text にする。
        let field = json!({
            "runs": [
                { "text": "hello " },
                {
                    "emoji": {
                        "emojiId": "emoji-1",
                        "shortcuts": [":wave:"],
                        "image": {
                            "thumbnails": [
                                { "url": "https://example.test/wave-24.png" },
                                { "url": "https://example.test/wave-48.png" }
                            ]
                        }
                    }
                },
                {
                    "emoji": {
                        "emojiId": "emoji-2",
                        "searchTerms": ["smile"],
                        "image": { "thumbnails": [] }
                    }
                },
                { "text": "" }
            ]
        });

        let fragments = parse_runs(Some(&field));

        assert_eq!(
            fragments,
            vec![
                Fragment::Text {
                    text: "hello ".to_string(),
                },
                Fragment::Emote {
                    id: "emoji-1".to_string(),
                    name: ":wave:".to_string(),
                    url: "https://example.test/wave-48.png".to_string(),
                },
                Fragment::Text {
                    text: "smile".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_action_detects_text_superchat_and_membership_kinds() {
        // addChatItemAction の代表的 renderer を kind と Amount に正規化する。
        let payload = json!({
            "continuationContents": {
                "liveChatContinuation": {
                    "actions": [
                        {
                            "addChatItemAction": {
                                "item": {
                                    "liveChatTextMessageRenderer": {
                                        "id": "normal-1",
                                        "timestampUsec": "1000000",
                                        "authorName": { "simpleText": "Alice" },
                                        "authorExternalChannelId": "UC_ALICE",
                                        "message": {
                                            "runs": [{ "text": "通常コメント" }]
                                        }
                                    }
                                }
                            }
                        },
                        {
                            "addChatItemAction": {
                                "item": {
                                    "liveChatPaidMessageRenderer": {
                                        "id": "superchat-1",
                                        "timestampUsec": "2000000",
                                        "authorName": { "simpleText": "Bob" },
                                        "authorExternalChannelId": "UC_BOB",
                                        "purchaseAmountText": { "simpleText": "¥1,000" },
                                        "message": {
                                            "runs": [{ "text": "ありがとう" }]
                                        }
                                    }
                                }
                            }
                        },
                        {
                            "addChatItemAction": {
                                "item": {
                                    "liveChatMembershipItemRenderer": {
                                        "id": "membership-1",
                                        "timestampUsec": "3000000",
                                        "authorName": { "simpleText": "Carol" },
                                        "authorExternalChannelId": "UC_CAROL",
                                        "headerSubtext": {
                                            "runs": [{ "text": "メンバーになりました" }]
                                        }
                                    }
                                }
                            }
                        }
                    ]
                }
            }
        });
        let paths: HashMap<String, String> = HashMap::new();
        let actions = extract_actions(&payload, &paths);

        let normal = parse_action(&actions[0], "video-1").expect("normal message");
        let superchat = parse_action(&actions[1], "video-1").expect("superchat message");
        let membership = parse_action(&actions[2], "video-1").expect("membership message");

        assert_eq!(normal.kind, MessageKind::Normal);
        assert_eq!(normal.plain_text(), "通常コメント");
        assert_eq!(normal.timestamp_ms, 1000);

        assert_eq!(superchat.kind, MessageKind::SuperChat);
        assert_eq!(superchat.plain_text(), "ありがとう");
        let amount = superchat.amount.as_ref().expect("superchat amount");
        assert_eq!(amount.raw_text.as_str(), "¥1,000");
        assert_eq!(amount.currency.as_str(), "¥");
        assert_eq!(amount.value, 1000.0);

        assert_eq!(membership.kind, MessageKind::Membership);
        assert_eq!(membership.plain_text(), "メンバーになりました");
        assert!(membership.amount.is_none());
    }

    #[test]
    fn author_badges_map_to_member_moderator_and_owner_roles() {
        // authorBadges は member/moderator/owner を Roles へ写す(owner は broadcaster)。
        let action = json!({
            "addChatItemAction": {
                "item": {
                    "liveChatTextMessageRenderer": {
                        "id": "badge-1",
                        "timestampUsec": "4000000",
                        "authorName": { "simpleText": "Dana" },
                        "authorExternalChannelId": "UC_DANA",
                        "authorBadges": [
                            {
                                "liveChatAuthorBadgeRenderer": {
                                    "tooltip": "Member",
                                    "customThumbnail": {
                                        "thumbnails": [
                                            { "url": "https://example.test/member.png" }
                                        ]
                                    }
                                }
                            },
                            {
                                "liveChatAuthorBadgeRenderer": {
                                    "tooltip": "Moderator",
                                    "icon": { "iconType": "MODERATOR" }
                                }
                            },
                            {
                                "liveChatAuthorBadgeRenderer": {
                                    "tooltip": "Owner",
                                    "icon": { "iconType": "OWNER" }
                                }
                            }
                        ],
                        "message": {
                            "runs": [{ "text": "badge check" }]
                        }
                    }
                }
            }
        });

        let msg = parse_action(&action, "video-1").expect("badge message");

        assert!(msg.author.roles.member);
        assert!(msg.author.roles.moderator);
        assert!(msg.author.roles.broadcaster);
        assert!(!msg.author.roles.subscriber);
        assert!(!msg.author.roles.vip);
        assert_eq!(msg.author.badges[0].kind.as_str(), "member");
        assert_eq!(
            msg.author.badges[0].image_url.as_deref(),
            Some("https://example.test/member.png")
        );
        assert_eq!(msg.author.badges[1].kind.as_str(), "moderator");
        assert_eq!(msg.author.badges[2].kind.as_str(), "broadcaster");
    }

    #[test]
    fn split_currency_value_handles_jpy_and_european_amounts() {
        // 金額文字列は日本円の桁区切りと欧州表記を数値化し、Amount は raw_text を温存する。
        let (currency, value) = split_currency_value("¥1,000");
        assert_eq!(currency, "¥");
        assert_eq!(value, 1000.0);

        let (currency, value) = split_currency_value("1.000,50");
        assert_eq!(currency, "");
        assert_eq!(value, 1000.50);

        let action = json!({
            "addChatItemAction": {
                "item": {
                    "liveChatPaidMessageRenderer": {
                        "id": "amount-1",
                        "timestampUsec": "5000000",
                        "authorName": { "simpleText": "Eve" },
                        "authorExternalChannelId": "UC_EVE",
                        "purchaseAmountText": { "simpleText": "1.000,50" }
                    }
                }
            }
        });

        let msg = parse_action(&action, "video-1").expect("paid message");
        let amount = msg.amount.as_ref().expect("amount");
        assert_eq!(amount.value, 1000.50);
        assert_eq!(amount.currency.as_str(), "");
        assert_eq!(amount.raw_text.as_str(), "1.000,50");
        assert_eq!(msg.fragments, vec![Fragment::text("1.000,50")]);
    }

    #[test]
    fn unknown_or_broken_actions_return_none_and_are_skipped() {
        // 未知 renderer や壊れた addChatItemAction は panic せず None になり、filter_map でスキップできる。
        let payload = json!({
            "continuationContents": {
                "liveChatContinuation": {
                    "actions": [
                        {
                            "addChatItemAction": {
                                "item": {
                                    "liveChatUnknownRenderer": {
                                        "id": "unknown-1"
                                    }
                                }
                            }
                        },
                        {
                            "addChatItemAction": {}
                        },
                        {
                            "replaceChatItemAction": {
                                "item": {}
                            }
                        },
                        {
                            "addChatItemAction": {
                                "item": {
                                    "liveChatTextMessageRenderer": {
                                        "id": "valid-1",
                                        "timestampUsec": "6000000",
                                        "authorName": { "simpleText": "Frank" },
                                        "authorExternalChannelId": "UC_FRANK",
                                        "message": {
                                            "runs": [{ "text": "kept" }]
                                        }
                                    }
                                }
                            }
                        }
                    ]
                }
            }
        });
        let paths: HashMap<String, String> = HashMap::new();
        let actions = extract_actions(&payload, &paths);

        assert!(parse_action(&actions[0], "video-1").is_none());
        assert!(parse_action(&actions[1], "video-1").is_none());
        assert!(parse_action(&actions[2], "video-1").is_none());

        let parsed = actions
            .iter()
            .filter_map(|action| parse_action(action, "video-1"))
            .collect::<Vec<_>>();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id.as_str(), "valid-1");
        assert_eq!(parsed[0].plain_text(), "kept");
    }
}
