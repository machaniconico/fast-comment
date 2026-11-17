//! PR4 受け入れテスト: 送り先 URL の検証と伏せ字(設計書 §3.4・§3.6・§5.6)。
//!
//! 実装者が満たす契約(付録 D に加えて、このファイルが前提にする物):
//! - `webhook::target::validate(format, raw) -> Result<(), UrlError>`。`UrlError: Debug`。
//!   `url` crate の正規化(ホストの小文字化、既定ポートの省略、前後の空白・制御文字の除去)に頼らず、
//!   生の文字列も検査する(`:443` の明示、前後の空白、改行は拒否)。
//! - `webhook::target::redact(raw) -> String` は `"<scheme>://<host>/…<生の URL の末尾 4 文字>"`。
//!   パースできない入力は固定文だけを返し、入力のどの部分も含まない。
//! - 900 バイトは UTF-8 のバイト数で数える(ちょうど 900 は可、901 は不可)。

use fast_comment_lib::integrations::WebhookFormat::{Discord, Generic};
use fast_comment_lib::webhook::target::{redact, validate};

fn ok(format: fast_comment_lib::integrations::WebhookFormat, url: &str) {
    assert!(validate(format, url).is_ok(), "受理されるはず: {url}");
}

fn ng(format: fast_comment_lib::integrations::WebhookFormat, url: &str) {
    assert!(validate(format, url).is_err(), "拒否されるはず: {url:?}");
}

#[test]
fn discord_accepts_discord_com_webhook_url() {
    ok(Discord, "https://discord.com/api/webhooks/123456789/abcDEF_-token");
}

#[test]
fn discord_accepts_discordapp_canary_ptb_and_versioned_path() {
    ok(Discord, "https://discordapp.com/api/webhooks/1/x");
    ok(Discord, "https://canary.discord.com/api/webhooks/1/x");
    ok(Discord, "https://ptb.discord.com/api/webhooks/1/x");
    ok(Discord, "https://discord.com/api/v10/webhooks/1/x");
    ok(Discord, "https://discord.com/api/v9/webhooks/1/x?wait=true");
}

#[test]
fn discord_rejects_http() {
    ng(Discord, "http://discord.com/api/webhooks/1/x");
}

#[test]
fn discord_rejects_other_hosts_and_lookalikes() {
    ng(Discord, "https://discord.com.evil.example/api/webhooks/1/x");
    ng(Discord, "https://evil-discord.com/api/webhooks/1/x");
    ng(Discord, "https://discord.com./api/webhooks/1/x");
    ng(Discord, "https://example.com/api/webhooks/1/x");
    ng(Discord, "https://sub.discord.com/api/webhooks/1/x");
}

#[test]
fn discord_rejects_non_webhook_path() {
    ng(Discord, "https://discord.com/channels/1/2");
    ng(Discord, "https://discord.com/");
    ng(Discord, "https://discord.com/api/vX/webhooks/1/x");
    ng(Discord, "https://discord.com/api/users/@me");
}

#[test]
fn discord_rejects_explicit_port_and_userinfo() {
    ng(Discord, "https://discord.com:8443/api/webhooks/1/x");
    ng(Discord, "https://discord.com:443/api/webhooks/1/x");
    ng(Discord, "https://user@discord.com/api/webhooks/1/x");
    ng(Discord, "https://user:pass@discord.com/api/webhooks/1/x");
}

#[test]
fn generic_accepts_https_any_host() {
    ok(Generic, "https://example.com/hook");
    ok(Generic, "https://nas.local/hook");
    ok(Generic, "https://8.8.8.8/hook");
    ok(Generic, "https://hooks.example.test:8443/in?token=abc");
}

#[test]
fn generic_accepts_http_for_localhost_and_loopback_and_private_literals() {
    for host in [
        "localhost",
        "127.0.0.1",
        "[::1]",
        "10.0.0.5",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.10",
        "169.254.1.1",
        "[fd12:3456::1]",
        "[fe80::1]",
    ] {
        ok(Generic, &format!("http://{host}/hook"));
        ok(Generic, &format!("http://{host}:5678/webhook/abc"));
    }
}

#[test]
fn generic_rejects_http_for_public_ip_and_hostnames() {
    for host in [
        "8.8.8.8",
        "172.32.0.1",
        "172.15.255.255",
        "11.0.0.1",
        "example.com",
        "nas.local",
        "192.168.1.10.evil.example",
        "localhost.evil.example",
        "[2001:db8::1]",
    ] {
        ng(Generic, &format!("http://{host}/hook"));
    }
}

#[test]
fn generic_rejects_non_http_schemes() {
    for url in ["file:///etc/passwd", "ftp://example.com/x", "javascript:alert(1)", "data:text/plain,hi"] {
        ng(Generic, url);
        ng(Discord, url);
    }
}

#[test]
fn rejects_userinfo_fragment_empty_host_and_whitespace() {
    for url in [
        "https://user:pass@example.com/x",
        "https://user@example.com/x",
        "https://example.com/x#frag",
        "https://example.com/x#",
        "https:///x",
        "https://",
        "https://exa mple.com/x",
        " https://example.com/x",
        "https://example.com/x ",
        "https://example.com/x\n",
        "https://example.com/\tx",
        "https://example.com/x\u{0007}",
        "",
    ] {
        ng(Generic, url);
    }
    ng(Discord, "https://discord.com/api/webhooks/1/x#frag");
    ng(Discord, " https://discord.com/api/webhooks/1/x");
    ng(Discord, "https://discord.com/api/webhooks/1/x\n");
}

#[test]
fn rejects_url_over_900_bytes_and_accepts_900() {
    let prefix = "https://example.com/";
    let at_limit = format!("{prefix}{}", "a".repeat(900 - prefix.len()));
    assert_eq!(at_limit.len(), 900);
    ok(Generic, &at_limit);
    ng(Generic, &format!("{at_limit}a"));

    // 全角文字はバイト数で数える(パスの % エンコード前の生の長さではなく、渡された文字列の長さ)。
    let multibyte = format!("{prefix}{}", "あ".repeat(300)); // 20 + 900 バイト
    assert!(multibyte.len() > 900);
    ng(Generic, &multibyte);

    let d_prefix = "https://discord.com/api/webhooks/1/";
    let d_ok = format!("{d_prefix}{}", "a".repeat(900 - d_prefix.len()));
    ok(Discord, &d_ok);
    ng(Discord, &format!("{d_ok}a"));
}

#[test]
fn validation_uses_parsed_host_not_string_prefix() {
    ng(Discord, "https://discord.com@evil.example/api/webhooks/1/x");
    ng(Discord, "https://discord.com:pass@evil.example/api/webhooks/1/x");
    ng(Generic, "http://192.168.1.10@evil.example/x");
    ng(Generic, "http://localhost@evil.example/x");
    ng(Generic, "http://192.168.1.10.evil.example/x");
}

#[test]
fn redact_hides_path_query_and_keeps_scheme_host_and_last_4() {
    let raw = "https://discord.com/api/webhooks/123456789/SECRETTOKEN-ab12?thread=SECRETQ";
    let r = redact(raw);
    assert_eq!(r, "https://discord.com/…RETQ");
    for secret in ["api", "webhooks", "123456789", "SECRETTOKEN", "thread"] {
        assert!(!r.contains(secret), "{r} に {secret} が残っている");
    }

    let r2 = redact("https://hook.example.test/in/SECRETPATH-abcd");
    assert_eq!(r2, "https://hook.example.test/…abcd");
}

#[test]
fn redact_of_unparsable_input_reveals_nothing() {
    let cases: [(&str, &[&str]); 4] = [
        ("ZZSECRETZZ-xyz987", &["SECRET", "xyz987", "z987"]),
        ("https://exa mple.com/SECRETPART-wxyz", &["SECRETPART", "wxyz", "exa mple"]),
        ("not a url SECRETWORD", &["SECRETWORD", "ORD", "not a url"]),
        ("", &[]),
    ];
    for (raw, leaks) in cases {
        let r = redact(raw);
        for leak in leaks {
            assert!(!r.contains(leak), "{raw:?} -> {r:?} に {leak} が残っている");
        }
    }
}
