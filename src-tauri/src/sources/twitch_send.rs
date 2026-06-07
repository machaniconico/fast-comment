//! Twitch チャット送信用の one-shot IRC-over-WebSocket 接続。
//!
//! 既存の匿名読み取り Source とは別接続で、1メッセージだけ投稿して閉じる。

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use futures_util::{SinkExt, StreamExt};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const TWITCH_WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";
const TWITCH_MAX_MESSAGE_CHARS: usize = 500;
const TWITCH_RESPONSE_WAIT: Duration = Duration::from_secs(3);
const TWITCH_SEND_TIMEOUT: Duration = Duration::from_secs(10);

struct TwitchSendRequest {
    channel: String,
    text: String,
    oauth: String,
    username: String,
}

/// Twitch へ認証付きで 1 メッセージを投稿する。
pub async fn send_twitch_message(
    channel: &str,
    text: &str,
    oauth: &str,
    username: &str,
) -> anyhow::Result<()> {
    let request = build_twitch_send_request(channel, text, oauth, username)?;
    timeout(TWITCH_SEND_TIMEOUT, send_twitch_message_inner(request))
        .await
        .map_err(|_| anyhow!("Twitchへの投稿がタイムアウトしました"))?
}

async fn send_twitch_message_inner(request: TwitchSendRequest) -> anyhow::Result<()> {
    let (ws_stream, _resp) = tokio_tungstenite::connect_async(TWITCH_WS_URL)
        .await
        .context("Twitch IRC WebSocketへの接続に失敗しました")?;
    let (mut write, mut read) = ws_stream.split();

    for line in irc_send_lines(&request) {
        write
            .send(irc_text(&line))
            .await
            .with_context(|| {
                format!(
                    "Twitch IRCコマンド送信に失敗しました: {}",
                    command_name(&line)
                )
            })?;
    }

    let response_result = timeout(TWITCH_RESPONSE_WAIT, async {
        loop {
            let Some(msg) = read.next().await else {
                return Ok(());
            };
            match msg.context("Twitch IRC応答の受信に失敗しました")? {
                WsMessage::Text(text) => {
                    for line in text.lines().map(str::trim_end).filter(|line| !line.is_empty()) {
                        if is_auth_failure_notice(line) {
                            bail!(
                                "Twitch認証に失敗しました。OAuthトークンとユーザー名を確認してください: {line}"
                            );
                        }
                        if let Some(reply) = pong_response(line) {
                            write
                                .send(irc_text(&reply))
                                .await
                                .context("Twitch PONG送信に失敗しました")?;
                        }
                    }
                }
                WsMessage::Ping(payload) => {
                    write
                        .send(WsMessage::Pong(payload))
                        .await
                        .context("Twitch WebSocket PONG送信に失敗しました")?;
                }
                WsMessage::Close(_) => return Ok(()),
                _ => {}
            }
        }
    })
    .await;

    if let Ok(Err(e)) = response_result {
        return Err(e);
    }

    let _ = write.send(WsMessage::Close(None)).await;
    Ok(())
}

fn build_twitch_send_request(
    channel: &str,
    text: &str,
    oauth: &str,
    username: &str,
) -> anyhow::Result<TwitchSendRequest> {
    Ok(TwitchSendRequest {
        channel: normalize_channel(channel)?,
        text: sanitize_message_text(text)?,
        oauth: normalize_oauth(oauth)?,
        username: normalize_username(username)?,
    })
}

fn normalize_channel(channel: &str) -> anyhow::Result<String> {
    let channel = channel.trim().trim_start_matches('#').to_lowercase();
    if channel.is_empty() {
        bail!("Twitchチャンネル名を入力してください");
    }
    if channel.chars().any(char::is_whitespace) {
        bail!("Twitchチャンネル名に空白や改行は使用できません");
    }
    Ok(channel)
}

fn sanitize_message_text(text: &str) -> anyhow::Result<String> {
    if text.trim().is_empty() {
        bail!("送信するコメント本文を入力してください");
    }

    let sanitized: String = text
        .chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .collect();
    if sanitized.trim().is_empty() {
        bail!("送信するコメント本文を入力してください");
    }
    if sanitized.chars().count() > TWITCH_MAX_MESSAGE_CHARS {
        bail!("Twitch投稿本文は500文字以内にしてください");
    }
    Ok(sanitized)
}

fn normalize_oauth(oauth: &str) -> anyhow::Result<String> {
    let oauth = oauth.trim();
    let token = oauth.strip_prefix("oauth:").unwrap_or(oauth);
    if token.is_empty() {
        bail!("Twitch OAuthトークンを設定してください");
    }
    if token.chars().any(char::is_whitespace) {
        bail!("Twitch OAuthトークンに空白や改行は使用できません");
    }
    if oauth.starts_with("oauth:") {
        Ok(oauth.to_string())
    } else {
        Ok(format!("oauth:{oauth}"))
    }
}

fn normalize_username(username: &str) -> anyhow::Result<String> {
    let username = username.trim().to_lowercase();
    if username.is_empty() {
        bail!("Twitchユーザー名を設定してください");
    }
    if username.chars().any(char::is_whitespace) {
        bail!("Twitchユーザー名に空白や改行は使用できません");
    }
    Ok(username)
}

fn privmsg_command(channel: &str, text: &str) -> String {
    format!("PRIVMSG #{channel} :{text}")
}

fn irc_send_lines(request: &TwitchSendRequest) -> [String; 4] {
    [
        format!("PASS {}", request.oauth),
        format!("NICK {}", request.username),
        format!("JOIN #{}", request.channel),
        privmsg_command(&request.channel, &request.text),
    ]
}

fn irc_text(s: &str) -> WsMessage {
    WsMessage::Text(s.to_string().into())
}

fn command_name(line: &str) -> &str {
    line.split_once(' ').map(|(name, _)| name).unwrap_or(line)
}

fn pong_response(line: &str) -> Option<String> {
    line.strip_prefix("PING ")
        .map(|payload| format!("PONG {payload}"))
}

fn is_auth_failure_notice(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("login authentication failed")
        || lower.contains("improperly formatted auth")
        || lower.contains("authentication failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_cr_lf_into_single_privmsg_line() {
        let text = sanitize_message_text("hello\r\nPRIVMSG #other :bad\nworld\r!").unwrap();
        assert_eq!(text, "helloPRIVMSG #other :badworld!");

        let command = privmsg_command("example", &text);
        assert_eq!(command, "PRIVMSG #example :helloPRIVMSG #other :badworld!");
        assert!(!command.contains('\r'));
        assert!(!command.contains('\n'));
    }

    #[test]
    fn normalizes_oauth_prefix() {
        assert_eq!(normalize_oauth("abc123").unwrap(), "oauth:abc123");
        assert_eq!(normalize_oauth(" oauth:def456 ").unwrap(), "oauth:def456");
    }

    #[test]
    fn normalizes_channel_hash_and_username_case() {
        assert_eq!(normalize_channel(" #Example_Channel ").unwrap(), "example_channel");
        assert_eq!(normalize_username(" FastCommentBot ").unwrap(), "fastcommentbot");
    }

    #[test]
    fn builds_irc_commands_in_twitch_send_order() {
        let request =
            build_twitch_send_request("#Example", "hello", "token123", "FastCommentBot").unwrap();

        assert_eq!(
            irc_send_lines(&request),
            [
                "PASS oauth:token123".to_string(),
                "NICK fastcommentbot".to_string(),
                "JOIN #example".to_string(),
                "PRIVMSG #example :hello".to_string(),
            ]
        );
    }
}
