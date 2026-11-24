//! Twitch 公式 Polls 受け入れテスト: スコープ(poll-pr2-twitch.md「スコープ」)。
use fast_comment_lib::sources::twitch_auth::{login_scopes, scopes_satisfied, TwitchOauthStatus, POLL_SCOPES, REQUIRED_SCOPES};

fn moderation_only() -> Vec<String> {
    REQUIRED_SCOPES.iter().map(|s| s.to_string()).collect()
}

#[test]
fn scopes_satisfied_checks_only_the_given_set() {
    let granted = moderation_only();
    assert!(scopes_satisfied(&REQUIRED_SCOPES, &granted));
    assert!(!scopes_satisfied(POLL_SCOPES, &granted));
}

#[test]
fn login_requests_moderation_and_poll_scopes_once_each() {
    let scopes = login_scopes();
    assert_eq!(scopes.len(), 3, "{scopes:?}");
    for want in ["moderator:manage:banned_users", "moderator:manage:chat_messages", "channel:manage:polls"] {
        assert_eq!(scopes.iter().filter(|s| **s == want).count(), 1, "{want} が 1 回ではない: {scopes:?}");
    }
}

#[test]
fn existing_moderation_only_token_reports_polls_scope_missing() {
    // 状態を組み立てる純関数は境界に無いので、scopes_satisfied と TwitchOauthStatus の直列化で代える。
    let granted = moderation_only();
    let status = TwitchOauthStatus {
        configured: true,
        connected: true,
        login: Some("streamer".into()),
        scopes_ok: scopes_satisfied(&REQUIRED_SCOPES, &granted),
        polls_scope_ok: scopes_satisfied(POLL_SCOPES, &granted),
    };
    assert!(status.scopes_ok);
    assert!(!status.polls_scope_ok);
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["scopesOk"], true);
    assert_eq!(json["pollsScopeOk"], false);
}
