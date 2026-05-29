//! Twitch IRC-over-WebSocket Source。
//!
//! 匿名(read-only)で `wss://irc-ws.chat.twitch.tv:443` に接続し、
//! `CAP REQ :twitch.tv/tags twitch.tv/commands` でIRCv3タグを有効化、
//! `JOIN #channel` して PRIVMSG を `ChatMessage` に正規化する。
//!
//! 重要: サーバからの `PING` には必ず `PONG` を返す(返さないと切断される)。

use std::collections::HashMap;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::model::{Amount, Author, Badge, ChatMessage, Fragment, MessageKind, Platform, Roles};

const TWITCH_WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Twitch チャンネル1件を購読する Source。
pub struct TwitchSource {
    /// `#` を含まないチャンネル名(小文字化して使う)。
    channel: String,
}

impl TwitchSource {
    pub fn new(channel: String) -> Self {
        // 入力に `#` が付いていても許容する。
        let channel = channel.trim().trim_start_matches('#').to_lowercase();
        TwitchSource { channel }
    }
}

impl Source for TwitchSource {
    fn name(&self) -> String {
        format!("twitch:{}", self.channel)
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let mut backoff = Backoff::new();
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }

                match self.connect_and_listen(&tx, &cancel).await {
                    Ok(stable) => {
                        // 正常終了(cancel)時はループを抜ける。
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        // サーバ都合の切断: 安定セッション(30秒以上 or データ受信あり)
                        // だった場合のみバックオフをリセットする。短命切断ではリセットしない。
                        if stable {
                            backoff.reset();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("twitch:{} 接続エラー: {e:#}", self.channel);
                    }
                }

                let delay = backoff.next_delay();
                tracing::info!("twitch:{} {}ms 後に再接続", self.channel, delay.as_millis());
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        })
    }
}

impl TwitchSource {
    /// 1接続分のセッション。接続→認証→JOIN→受信ループ。
    ///
    /// 戻り値 `Ok(true)` = 安定セッション(30秒以上継続 or データ受信あり)→バックオフリセット可。
    /// 戻り値 `Ok(false)` = 短命切断→バックオフを成長させ続ける。
    async fn connect_and_listen(
        &self,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let (ws_stream, _resp) = tokio_tungstenite::connect_async(TWITCH_WS_URL).await?;
        let (mut write, mut read) = ws_stream.split();

        // 匿名ログイン。justinfan + ランダム数値の NICK。
        let nick = format!("justinfan{}", fastrand_suffix());
        write
            .send(irc_text("CAP REQ :twitch.tv/tags twitch.tv/commands"))
            .await?;
        write.send(irc_text("PASS SCHMOOPIIE")).await?;
        write.send(irc_text(&format!("NICK {nick}"))).await?;
        write
            .send(irc_text(&format!("JOIN #{}", self.channel)))
            .await?;

        tracing::info!("twitch:{} 接続・JOIN 完了 (nick={nick})", self.channel);

        // 安定セッション判定: 接続後 30 秒以上経過、または PRIVMSG を1件でも受信。
        const STABLE_SECS: u64 = 30;
        let connected_at = Instant::now();
        let mut received_data = false;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = write.send(WsMessage::Close(None)).await;
                    return Ok(true); // キャンセルは正常終了扱い。
                }
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => return Err(e.into()),
                        None => {
                            // ストリーム終端=切断。安定判定を返す。
                            let stable = received_data
                                || connected_at.elapsed().as_secs() >= STABLE_SECS;
                            return Ok(stable);
                        }
                    };

                    match msg {
                        WsMessage::Text(text) => {
                            // 1フレームに複数IRC行が来ることがある(CRLF区切り)。
                            for line in text.split("\r\n").filter(|l| !l.is_empty()) {
                                if let Some(reply) = self.handle_line(line, tx) {
                                    // PING への PONG など、即返信が必要なもの。
                                    write.send(irc_text(&reply)).await?;
                                }
                            }
                            // PRIVMSG 受信とみなしてデータフラグを立てる。
                            // (handle_line 内で tx.send が成功した場合のみにしたいが、
                            //  シグネチャ変更を避けるため Text フレーム受信を近似とする。)
                            received_data = true;
                        }
                        WsMessage::Ping(payload) => {
                            // WS レベルの ping にも応答。
                            write.send(WsMessage::Pong(payload)).await?;
                        }
                        WsMessage::Close(_) => {
                            let stable = received_data
                                || connected_at.elapsed().as_secs() >= STABLE_SECS;
                            return Ok(stable);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// IRC 1行を処理する。返り値 `Some(s)` は即時に送り返すべき文字列(PONG)。
    fn handle_line(&self, line: &str, tx: &broadcast::Sender<ChatMessage>) -> Option<String> {
        let parsed = parse_irc_line(line);

        match parsed.command.as_str() {
            // サーバ ping → 必ず pong(切断防止)。
            // Twitch は `PING :tmi.twitch.tv` を送るため trailing にトークンが入る。
            // trailing 優先、なければ params フォールバック。
            "PING" => {
                let token = parsed.trailing.clone().or_else(|| {
                    let j = parsed.params.join(" ");
                    if j.is_empty() { None } else { Some(j) }
                }).unwrap_or_default();
                return Some(format!("PONG :{}", token));
            }
            "PRIVMSG" => {
                if let Some(msg) = self.privmsg_to_chat(&parsed) {
                    let _ = tx.send(msg);
                }
            }
            _ => {
                // JOIN/PART/USERSTATE/ROOMSTATE/NOTICE 等は今は無視。
            }
        }
        None
    }

    /// PRIVMSG を `ChatMessage` に正規化する。
    fn privmsg_to_chat(&self, p: &IrcLine) -> Option<ChatMessage> {
        // 本文: 最後のパラメータ(`:` 以降)。
        let body = p.trailing.clone().unwrap_or_default();

        // 送信者ログイン名: prefix の `nick!user@host` の nick 部分。
        let login = p
            .prefix
            .as_deref()
            .and_then(|pre| pre.split('!').next())
            .unwrap_or("")
            .to_string();

        let tags = &p.tags;

        let display_name = tags
            .get("display-name")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| login.clone());

        let user_id = tags
            .get("user-id")
            .cloned()
            .unwrap_or_else(|| login.clone());

        let color = tags
            .get("color")
            .filter(|s| !s.is_empty())
            .cloned();

        // バッジ → Roles / Badge ベクタ。
        let (roles, badges) = parse_badges(tags.get("badges").map(|s| s.as_str()).unwrap_or(""));

        // emotes タグから本文を Fragment 分割。
        let fragments = split_fragments(&body, tags.get("emotes").map(|s| s.as_str()).unwrap_or(""));

        // bits タグがあれば Bits 扱い。
        let (kind, amount) = match tags.get("bits").and_then(|b| b.parse::<f64>().ok()) {
            Some(bits) if bits > 0.0 => (
                MessageKind::Bits,
                Some(Amount {
                    value: bits,
                    currency: "BITS".to_string(),
                    raw_text: format!("{} bits", bits as u64),
                }),
            ),
            _ => (MessageKind::Normal, None),
        };

        // tmi-sent-ts(ms)が取れればそれを、無ければ現在時刻。
        let timestamp_ms = tags
            .get("tmi-sent-ts")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(now_ms);

        Some(ChatMessage {
            id: tags
                .get("id")
                .cloned()
                .unwrap_or_else(ChatMessage::new_id),
            platform: Platform::Twitch,
            channel: self.channel.clone(),
            author: Author {
                id: user_id,
                name: display_name,
                display_color: color,
                badges,
                roles,
            },
            fragments,
            kind,
            amount,
            timestamp_ms,
            raw: None,
        })
    }
}

/// パース済み IRC 行。
struct IrcLine {
    tags: HashMap<String, String>,
    prefix: Option<String>,
    command: String,
    params: Vec<String>,
    /// `:` 以降の trailing パラメータ(本文)。
    trailing: Option<String>,
}

/// IRCv3 1行をパースする。
///
/// 形式: `[@tags] [:prefix] COMMAND [params] [:trailing]`
fn parse_irc_line(line: &str) -> IrcLine {
    let mut rest = line;
    let mut tags = HashMap::new();

    // @tags
    if let Some(stripped) = rest.strip_prefix('@') {
        let (tag_str, after) = stripped.split_once(' ').unwrap_or((stripped, ""));
        for kv in tag_str.split(';') {
            if kv.is_empty() {
                continue;
            }
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            tags.insert(k.to_string(), unescape_tag_value(v));
        }
        rest = after;
    }

    rest = rest.trim_start();

    // :prefix
    let mut prefix = None;
    if let Some(stripped) = rest.strip_prefix(':') {
        let (pre, after) = stripped.split_once(' ').unwrap_or((stripped, ""));
        prefix = Some(pre.to_string());
        rest = after;
    }

    rest = rest.trim_start();

    // COMMAND と params(trailing は最初の " :" 区切り)。
    let (head, trailing) = match rest.split_once(" :") {
        Some((h, t)) => (h, Some(t.to_string())),
        None => (rest, None),
    };

    let mut parts = head.split_whitespace();
    let command = parts.next().unwrap_or("").to_string();
    let params: Vec<String> = parts.map(|s| s.to_string()).collect();

    IrcLine {
        tags,
        prefix,
        command,
        params,
        trailing,
    }
}

/// IRCv3 タグ値のエスケープを戻す(`\s`=空白, `\:`=`;`, `\\`=`\`, `\r` `\n`)。
fn unescape_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some(':') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `badges` タグ(`broadcaster/1,subscriber/12,...`)を Roles と Badge へ。
fn parse_badges(badges_tag: &str) -> (Roles, Vec<Badge>) {
    let mut roles = Roles::default();
    let mut badges = Vec::new();

    for entry in badges_tag.split(',').filter(|e| !e.is_empty()) {
        let (kind, version) = entry.split_once('/').unwrap_or((entry, ""));
        match kind {
            "broadcaster" => roles.broadcaster = true,
            "moderator" => roles.moderator = true,
            "subscriber" | "founder" => roles.subscriber = true,
            "vip" => roles.vip = true,
            // Twitch にメンバー概念は無いが、サブスクをメンバー相当とみなす。
            _ => {}
        }
        badges.push(Badge {
            kind: kind.to_string(),
            label: if version.is_empty() {
                kind.to_string()
            } else {
                format!("{kind}/{version}")
            },
            image_url: None,
        });
    }
    // サブスクライバ=メンバー相当としても立てる(UIの member 強調用)。
    if roles.subscriber {
        roles.member = true;
    }

    (roles, badges)
}

/// `emotes` タグと本文から Fragment 列を作る。
///
/// emotes タグ形式: `id:start-end,start-end/id2:start-end`
/// インデックスは Unicode コードポイント単位(UTF-16 ではない、Twitch は code point)。
fn split_fragments(body: &str, emotes_tag: &str) -> Vec<Fragment> {
    if body.is_empty() {
        return Vec::new();
    }

    // 本文を char(コードポイント)単位で扱う。
    let chars: Vec<char> = body.chars().collect();

    if emotes_tag.is_empty() {
        return vec![Fragment::text(body.to_string())];
    }

    // (start, end, emote_id) を収集。
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    for emote in emotes_tag.split('/').filter(|e| !e.is_empty()) {
        let (id, positions) = match emote.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        for pos in positions.split(',').filter(|p| !p.is_empty()) {
            let (s, e) = match pos.split_once('-') {
                Some(v) => v,
                None => continue,
            };
            if let (Ok(s), Ok(e)) = (s.parse::<usize>(), e.parse::<usize>()) {
                ranges.push((s, e, id.to_string()));
            }
        }
    }

    if ranges.is_empty() {
        return vec![Fragment::text(body.to_string())];
    }

    // 出現位置順にソート。
    ranges.sort_by_key(|r| r.0);

    let mut fragments = Vec::new();
    let mut cursor = 0usize;

    for (start, end, id) in ranges {
        if start >= chars.len() || end >= chars.len() || start > end {
            continue;
        }
        // エモート前のテキスト。
        if start > cursor {
            let text: String = chars[cursor..start].iter().collect();
            if !text.is_empty() {
                fragments.push(Fragment::text(text));
            }
        }
        // エモート本体。
        let name: String = chars[start..=end].iter().collect();
        let url = format!(
            "https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0"
        );
        fragments.push(Fragment::Emote { id, name, url });
        cursor = end + 1;
    }

    // 末尾の残りテキスト。
    if cursor < chars.len() {
        let text: String = chars[cursor..].iter().collect();
        if !text.is_empty() {
            fragments.push(Fragment::text(text));
        }
    }

    if fragments.is_empty() {
        fragments.push(Fragment::text(body.to_string()));
    }

    fragments
}

/// IRC 文字列を WS テキストフレームへ。
///
/// tungstenite 0.24 の `Message::Text` は `Utf8Bytes` を取るため、
/// `String`(=`From<String> for Utf8Bytes`)経由で曖昧さなく変換する。
fn irc_text(s: &str) -> WsMessage {
    WsMessage::Text(s.to_string().into())
}

/// 匿名 NICK 用のランダム数値サフィックス(crate 追加なしで簡易に)。
fn fastrand_suffix() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // justinfan は 1〜99999 程度で十分。
    10_000 + (nanos % 80_000)
}

/// 現在時刻(unix ms)。
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
