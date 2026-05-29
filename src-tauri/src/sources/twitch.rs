//! Twitch IRC-over-WebSocket Source。
//!
//! 匿名(read-only)で `wss://irc-ws.chat.twitch.tv:443` に接続し、
//! `CAP REQ :twitch.tv/tags twitch.tv/commands` でIRCv3タグを有効化、
//! `JOIN #channel` して PRIVMSG を `ChatMessage` に正規化する。
//!
//! 重要: サーバからの `PING` には必ず `PONG` を返す(返さないと切断される)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::model::{Amount, Author, Badge, ChatMessage, Fragment, MessageKind, Platform, Roles};

const TWITCH_WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";
static NICK_COUNTER: AtomicUsize = AtomicUsize::new(0);

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

        // 匿名ログイン。justinfan + 数値サフィックスの NICK。
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
                                let handled = self.handle_line(line, tx);
                                if handled.emitted_privmsg {
                                    received_data = true;
                                }
                                if let Some(reply) = handled.reply {
                                    // PING への PONG など、即返信が必要なもの。
                                    write.send(irc_text(&reply)).await?;
                                }
                            }
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

    /// IRC 1行を処理する。
    fn handle_line(&self, line: &str, tx: &broadcast::Sender<ChatMessage>) -> HandleLineResult {
        let parsed = parse_irc_line(line);
        let mut result = HandleLineResult::default();

        match parsed.command.as_str() {
            // サーバ ping → 必ず pong(切断防止)。
            // Twitch は `PING :tmi.twitch.tv` を送るため trailing にトークンが入る。
            // trailing 優先、なければ params フォールバック。
            "PING" => {
                let token = parsed.trailing.clone().or_else(|| {
                    let j = parsed.params.join(" ");
                    if j.is_empty() { None } else { Some(j) }
                }).unwrap_or_default();
                result.reply = Some(format!("PONG :{}", token));
            }
            "PRIVMSG" => {
                if let Some(msg) = self.privmsg_to_chat(&parsed) {
                    result.emitted_privmsg = tx.send(msg).is_ok();
                }
            }
            _ => {
                // JOIN/PART/USERSTATE/ROOMSTATE/NOTICE 等は今は無視。
            }
        }
        result
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

#[derive(Default)]
struct HandleLineResult {
    /// 即時に送り返すべきIRC文字列(PONGなど)。
    reply: Option<String>,
    /// PRIVMSG を ChatMessage に正規化し、broadcast 送信に成功したか。
    emitted_privmsg: bool,
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
/// インデックスは Twitch 仕様の UTF-16 コードユニット単位。
fn split_fragments(body: &str, emotes_tag: &str) -> Vec<Fragment> {
    if body.is_empty() {
        return Vec::new();
    }

    let utf16: Vec<u16> = body.encode_utf16().collect();

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
        if start >= utf16.len() || end >= utf16.len() || start > end {
            continue;
        }
        // エモート本体。
        let name = match String::from_utf16(&utf16[start..=end]) {
            Ok(name) => name,
            Err(_) => continue,
        };
        // エモート前のテキスト。
        if start > cursor {
            let text = match String::from_utf16(&utf16[cursor..start]) {
                Ok(text) => text,
                Err(_) => continue,
            };
            if !text.is_empty() {
                fragments.push(Fragment::text(text));
            }
        }
        let url = format!(
            "https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0"
        );
        fragments.push(Fragment::Emote { id, name, url });
        cursor = end + 1;
    }

    // 末尾の残りテキスト。
    if cursor < utf16.len() {
        let text = match String::from_utf16(&utf16[cursor..]) {
            Ok(text) => text,
            Err(_) => body.to_string(),
        };
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

/// 匿名 NICK 用の数値サフィックス(crate 追加なしで時刻と単調カウンタを混ぜる)。
fn fastrand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let counter = (NICK_COUNTER.fetch_add(1, Ordering::Relaxed) as u64) & 0xffff;
    ((millis & 0x0fffffff) << 16) | counter
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
    use super::*;
    use crate::model::{ChatMessage, Fragment, MessageKind, Platform};
    use tokio::sync::broadcast;

    const TAGGED_BITS_PRIVMSG: &str = concat!(
        "@badge-info=subscriber/12;badges=broadcaster/1,moderator/1,subscriber/12,vip/1;",
        "client-nonce=0123456789abcdef;color=#1E90FF;display-name=RealUser;",
        "emotes=25:9-13;first-msg=0;flags=;",
        "id=9f60f9af-f7d-4bc3-bf55-1c15b310d8fb;mod=1;returning-chatter=0;",
        "room-id=111111;subscriber=1;tmi-sent-ts=1700000000000;turbo=0;",
        "user-id=12345;user-type=;bits=100 ",
        ":realuser!realuser@realuser.tmi.twitch.tv PRIVMSG #fastcomment :cheer100 Kappa hello",
    );

    const UTF16_EMOTE_PRIVMSG: &str = concat!(
        "@badge-info=;badges=;client-nonce=abcdef0123456789;color=;display-name=UTF16User;",
        "emotes=25:2-6/88:13-20;first-msg=0;flags=;",
        "id=0d7f3fd2-6561-4d70-920f-3c9a6c8a4471;mod=0;returning-chatter=0;",
        "room-id=111111;subscriber=0;tmi-sent-ts=1700000000001;turbo=0;",
        "user-id=67890;user-type= ",
        ":utf16user!utf16user@utf16user.tmi.twitch.tv PRIVMSG #fastcomment :😀Kappa test PogChamp",
    );

    const NORMAL_PRIVMSG: &str = concat!(
        "@badge-info=;badges=;client-nonce=fedcba9876543210;color=#FF69B4;display-name=NormalUser;",
        "emotes=;first-msg=0;flags=;",
        "id=66b8c9cf-b545-4f8e-8c4b-9c8b17b99418;mod=0;returning-chatter=0;",
        "room-id=111111;subscriber=0;tmi-sent-ts=1700000000002;turbo=0;",
        "user-id=22222;user-type= ",
        ":normaluser!normaluser@normaluser.tmi.twitch.tv PRIVMSG #fastcomment :hello chat",
    );

    const FOUNDER_PRIVMSG: &str = concat!(
        "@badge-info=founder/0;badges=founder/0;client-nonce=0011223344556677;color=;",
        "display-name=FounderUser;emotes=;first-msg=0;flags=;",
        "id=48183d84-165d-4020-a297-48c1f7f8f1af;mod=0;returning-chatter=0;",
        "room-id=111111;subscriber=1;tmi-sent-ts=1700000000003;turbo=0;",
        "user-id=33333;user-type= ",
        ":founderuser!founderuser@founderuser.tmi.twitch.tv PRIVMSG #fastcomment :founder hello",
    );

    fn source() -> TwitchSource {
        TwitchSource::new("fastcomment".to_string())
    }

    fn tag<'a>(line: &'a IrcLine, key: &str) -> &'a str {
        line.tags.get(key).map(String::as_str).unwrap()
    }

    fn chat_from(line: &str) -> ChatMessage {
        source()
            .privmsg_to_chat(&parse_irc_line(line))
            .expect("PRIVMSG should parse into ChatMessage")
    }

    fn emote_url(id: &str) -> String {
        format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0")
    }

    #[test]
    fn parses_privmsg_tags_into_fields() {
        // 実際のTwitch PRIVMSG行から主要IRCv3タグを抽出できることを確認する。
        let parsed = parse_irc_line(TAGGED_BITS_PRIVMSG);

        assert_eq!(parsed.command, "PRIVMSG");
        assert_eq!(
            parsed.prefix.as_deref(),
            Some("realuser!realuser@realuser.tmi.twitch.tv")
        );
        assert_eq!(parsed.params, vec!["#fastcomment".to_string()]);
        assert_eq!(parsed.trailing.as_deref(), Some("cheer100 Kappa hello"));
        assert_eq!(tag(&parsed, "display-name"), "RealUser");
        assert_eq!(tag(&parsed, "color"), "#1E90FF");
        assert_eq!(
            tag(&parsed, "badges"),
            "broadcaster/1,moderator/1,subscriber/12,vip/1"
        );
        assert_eq!(tag(&parsed, "emotes"), "25:9-13");
        assert_eq!(tag(&parsed, "bits"), "100");
        assert_eq!(tag(&parsed, "id"), "9f60f9af-f7d-4bc3-bf55-1c15b310d8fb");
        assert_eq!(tag(&parsed, "tmi-sent-ts"), "1700000000000");

        let msg = source()
            .privmsg_to_chat(&parsed)
            .expect("PRIVMSG should parse into ChatMessage");
        assert_eq!(msg.platform, Platform::Twitch);
        assert_eq!(msg.channel, "fastcomment");
        assert_eq!(msg.id, "9f60f9af-f7d-4bc3-bf55-1c15b310d8fb");
        assert_eq!(msg.author.id, "12345");
        assert_eq!(msg.author.name, "RealUser");
        assert_eq!(msg.author.display_color.as_deref(), Some("#1E90FF"));
        assert_eq!(msg.timestamp_ms, 1700000000000);
    }

    #[test]
    fn badges_tag_sets_roles_and_founder_as_subscriber() {
        // badgesタグを共通Rolesへ変換し、founderはsubscriber相当として扱う。
        let msg = chat_from(TAGGED_BITS_PRIVMSG);
        let roles = &msg.author.roles;
        let badges = &msg.author.badges;

        assert!(roles.broadcaster);
        assert!(roles.moderator);
        assert!(roles.subscriber);
        assert!(roles.member);
        assert!(roles.vip);
        assert_eq!(badges.len(), 4);
        assert_eq!(badges[0].kind, "broadcaster");
        assert_eq!(badges[1].kind, "moderator");
        assert_eq!(badges[2].label, "subscriber/12");
        assert_eq!(badges[3].label, "vip/1");

        let founder = chat_from(FOUNDER_PRIVMSG);
        let founder_roles = &founder.author.roles;
        let founder_badges = &founder.author.badges;
        assert!(founder_roles.subscriber);
        assert!(founder_roles.member);
        assert!(!founder_roles.broadcaster);
        assert!(!founder_roles.moderator);
        assert!(!founder_roles.vip);
        assert_eq!(founder_badges[0].kind, "founder");
        assert_eq!(founder_badges[0].label, "founder/0");
    }

    #[test]
    fn splits_emotes_on_utf16_code_unit_boundaries() {
        // サロゲートペアを含む本文でもTwitchのUTF-16境界でemote分割する。
        let parsed = parse_irc_line(UTF16_EMOTE_PRIVMSG);
        let expected = vec![
            Fragment::text("😀"),
            Fragment::Emote {
                id: "25".to_string(),
                name: "Kappa".to_string(),
                url: emote_url("25"),
            },
            Fragment::text(" test "),
            Fragment::Emote {
                id: "88".to_string(),
                name: "PogChamp".to_string(),
                url: emote_url("88"),
            },
        ];

        let fragments = split_fragments(
            parsed.trailing.as_deref().unwrap(),
            tag(&parsed, "emotes"),
        );
        assert_eq!(fragments, expected);

        let msg = source()
            .privmsg_to_chat(&parsed)
            .expect("PRIVMSG should parse into ChatMessage");
        assert_eq!(msg.fragments, expected);
        assert_eq!(msg.plain_text(), "😀Kappa test PogChamp");
    }

    #[test]
    fn bits_tag_creates_bits_message_kind_and_amount() {
        // bitsタグ付きPRIVMSGはBits種別とAmountへ正規化される。
        let msg = chat_from(TAGGED_BITS_PRIVMSG);

        assert_eq!(msg.kind, MessageKind::Bits);
        let amount = msg.amount.expect("bits tag should create Amount");
        assert_eq!(amount.value, 100.0);
        assert_eq!(amount.currency, "BITS");
        assert_eq!(amount.raw_text, "100 bits");
    }

    #[test]
    fn ping_returns_pong_reply() {
        // TwitchのPING行に対して即時返信用のPONG文字列を返す。
        let (tx, _rx) = broadcast::channel(1);
        let handled = source().handle_line("PING :tmi.twitch.tv", &tx);

        assert_eq!(handled.reply.as_deref(), Some("PONG :tmi.twitch.tv"));
        assert!(!handled.emitted_privmsg);
    }

    #[test]
    fn handle_line_emits_only_privmsg() {
        // 通常PRIVMSGのみ送信済み扱いになり、CAP ACKや001 welcomeは無視される。
        let (tx, mut rx) = broadcast::channel(8);
        let source = source();

        let privmsg = source.handle_line(NORMAL_PRIVMSG, &tx);
        assert!(privmsg.emitted_privmsg);
        assert!(privmsg.reply.is_none());

        let emitted = rx.try_recv().expect("PRIVMSG should be broadcast");
        assert_eq!(emitted.kind, MessageKind::Normal);
        assert_eq!(emitted.plain_text(), "hello chat");

        let cap_ack =
            source.handle_line(":tmi.twitch.tv CAP * ACK :twitch.tv/tags twitch.tv/commands", &tx);
        assert!(!cap_ack.emitted_privmsg);
        assert!(cap_ack.reply.is_none());

        let welcome = source.handle_line(":tmi.twitch.tv 001 justinfan123 :Welcome, GLHF!", &tx);
        assert!(!welcome.emitted_privmsg);
        assert!(welcome.reply.is_none());
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}
