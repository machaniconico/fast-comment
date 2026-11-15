//! PR1 受け入れテスト: API ゲート・認証・Host / Origin・応答の体裁(設計書 §5.3)。

use crate::common::*;
use axum::http::{HeaderValue, Method, StatusCode};
use fast_comment_lib::api::{self, ApiGate};
use std::collections::HashSet;

const STATUS: &str = "/api/v1/status";

// ---- 無効時 ----

#[tokio::test]
async fn api_disabled_status_returns_404_with_empty_body() {
    let app = api_app(&ApiGate::closed());
    let r = send(&app, get(STATUS)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.is_empty(), "本文は空: {:?}", String::from_utf8_lossy(&r.body));
}

#[tokio::test]
async fn api_disabled_returns_404_even_with_valid_token() {
    let (gate, token) = open_gate();
    gate.close();
    let app = api_app(&gate);
    let r = send(&app, get(STATUS).bearer(&token)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn api_disabled_returns_404_for_bad_host_and_origin() {
    let app = api_app(&ApiGate::closed());
    let bad_host = send(&app, get(STATUS).host("evil.example:47891")).await;
    assert_eq!(bad_host.status, StatusCode::NOT_FOUND);
    let origin = send(&app, get(STATUS).origin("http://evil.example")).await;
    assert_eq!(origin.status, StatusCode::NOT_FOUND);
    let both = send(&app, get(STATUS).host("evil.example:47891").origin("null")).await;
    assert_eq!(both.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_disabled_returns_404_for_every_method_and_unknown_path() {
    let app = api_app(&ApiGate::closed());
    let methods = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::OPTIONS,
        Method::HEAD,
    ];
    for path in [STATUS, "/api/v1/nope", "/api", "/api/"] {
        for m in &methods {
            let r = send(&app, req(m.clone(), path)).await;
            assert_eq!(r.status, StatusCode::NOT_FOUND, "{m} {path}");
            assert!(r.body.is_empty(), "{m} {path} の本文は空");
        }
    }
}

// ---- 認証 ----

#[tokio::test]
async fn missing_authorization_returns_401_with_www_authenticate_bearer() {
    let (gate, _t) = open_gate();
    let r = send(&api_app(&gate), get(STATUS)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.header("www-authenticate").as_deref(), Some("Bearer"));
}

#[tokio::test]
async fn wrong_token_returns_401_unauthorized_code() {
    let (gate, _t) = open_gate();
    let other = api::generate_token();
    let r = send(&api_app(&gate), get(STATUS).bearer(&other)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.error_code(), "unauthorized");
}

#[tokio::test]
async fn valid_bearer_returns_200_status_json() {
    let (gate, token) = open_gate();
    let r = send(&api_app(&gate), get(STATUS).bearer(&token)).await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(j["app"], "fast-comment");
    assert_eq!(j["apiVersion"], 1);
    assert_eq!(j["version"], env!("CARGO_PKG_VERSION"));
    let epoch = j["epoch"].as_str().expect("epoch は文字列");
    assert_eq!(epoch.len(), 12, "epoch は 12 桁: {epoch}");
    assert!(epoch.chars().all(|c| c.is_ascii_hexdigit()), "epoch は 16 進: {epoch}");
}

#[tokio::test]
async fn bearer_scheme_is_case_insensitive() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    for scheme in ["bearer", "BEARER", "Bearer", "bEaReR"] {
        let r = send(&app, get(STATUS).authorization(&format!("{scheme} {token}"))).await;
        assert_eq!(r.status, StatusCode::OK, "scheme {scheme}");
    }
}

#[tokio::test]
async fn basic_scheme_is_rejected_401() {
    let (gate, token) = open_gate();
    let r = send(&api_app(&gate), get(STATUS).authorization(&format!("Basic {token}"))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_in_query_is_not_accepted() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    for key in ["token", "access_token"] {
        let r = send(&app, get(&format!("{STATUS}?{key}={token}"))).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "?{key}=");
    }
}

#[tokio::test]
async fn token_with_extra_or_missing_chars_is_rejected() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    let cases = [
        format!("Bearer {token}x"),
        format!("Bearer {}", &token[..45]),
        "Bearer ".to_string(),
        "Bearer".to_string(),
        format!("Bearer  {token}"),
        format!("Bearer {token} "),
    ];
    for value in cases {
        let r = send(&app, get(STATUS).authorization(&value)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "Authorization: {value:?}");
    }
}

#[tokio::test]
async fn authorization_over_512_bytes_is_rejected_401() {
    // トークン自体が長いゲートでも、512 バイト超のヘッダは長さだけで落とす。
    let gate = ApiGate::closed();
    let long = "a".repeat(600);
    gate.open(&long);
    let r = send(&api_app(&gate), get(STATUS).bearer(&long)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_utf8_authorization_is_rejected_401() {
    let (gate, token) = open_gate();
    let mut value = format!("Bearer {token}").into_bytes();
    value.push(0xff);
    let r = send(&api_app(&gate), get(STATUS).raw_header("authorization", &value)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn duplicate_authorization_headers_are_rejected_401() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    let good = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    let bad = HeaderValue::from_static("Bearer not-the-token");
    let good_first = get(STATUS)
        .push_header("authorization", good.clone())
        .push_header("authorization", bad.clone());
    assert_eq!(send(&app, good_first).await.status, StatusCode::UNAUTHORIZED);
    let bad_first = get(STATUS).push_header("authorization", bad).push_header("authorization", good.clone());
    assert_eq!(send(&app, bad_first).await.status, StatusCode::UNAUTHORIZED);
    let twice = get(STATUS).push_header("authorization", good.clone()).push_header("authorization", good);
    assert_eq!(send(&app, twice).await.status, StatusCode::UNAUTHORIZED);
}

#[test]
fn gate_verify_false_when_closed_or_token_differs() {
    let gate = ApiGate::closed();
    let token = api::generate_token();
    assert!(!gate.verify(&token), "閉じたゲートは何も通さない");
    gate.open(&token);
    assert!(gate.verify(&token));
    assert!(!gate.verify(&format!("{token}x")));
    assert!(!gate.verify(&api::generate_token()));
    assert!(!gate.verify(""));
    gate.close();
    assert!(!gate.verify(&token), "閉じた後は正しいトークンも通さない");
}

#[tokio::test]
async fn regenerated_token_invalidates_old_token_immediately() {
    let (gate, old) = open_gate();
    let app = api_app(&gate);
    assert_eq!(send(&app, get(STATUS).bearer(&old)).await.status, StatusCode::OK);
    let new = api::generate_token();
    gate.open(&new);
    assert_eq!(send(&app, get(STATUS).bearer(&old)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, get(STATUS).bearer(&new)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn closing_gate_turns_valid_token_into_404() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    assert_eq!(send(&app, get(STATUS).bearer(&token)).await.status, StatusCode::OK);
    gate.close();
    assert!(!gate.is_open());
    assert_eq!(send(&app, get(STATUS).bearer(&token)).await.status, StatusCode::NOT_FOUND);
}

#[test]
fn generated_token_is_fc_prefix_plus_43_base64url_chars() {
    let t = api::generate_token();
    assert_eq!(t.len(), 46, "{t}");
    let rest = t.strip_prefix("fc_").expect("fc_ で始まる");
    assert_eq!(rest.len(), 43);
    assert!(
        rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "base64url 文字のみ(パディングなし): {rest}"
    );
}

#[test]
fn generated_tokens_do_not_repeat_in_1000_draws() {
    let set: HashSet<String> = (0..1000).map(|_| api::generate_token()).collect();
    assert_eq!(set.len(), 1000);
}

#[test]
fn gate_debug_output_does_not_contain_token() {
    let (gate, token) = open_gate();
    let dbg = format!("{gate:?} {gate:#?}");
    assert!(!dbg.contains(&token));
    assert!(!dbg.contains(&token[3..]), "接頭辞を除いた本体も出さない");
}

// ---- Host ----

async fn status_with_host(host: &str) -> crate::common::Resp {
    let (gate, token) = open_gate();
    send(&api_app(&gate), get(STATUS).host(host).bearer(&token)).await
}

#[tokio::test]
async fn host_127_0_0_1_with_bound_port_is_allowed() {
    assert_eq!(status_with_host(&format!("127.0.0.1:{PORT}")).await.status, StatusCode::OK);
}

#[tokio::test]
async fn host_localhost_is_allowed_case_insensitively() {
    assert_eq!(status_with_host(&format!("localhost:{PORT}")).await.status, StatusCode::OK);
    assert_eq!(status_with_host(&format!("LocalHost:{PORT}")).await.status, StatusCode::OK);
}

#[tokio::test]
async fn host_ipv6_loopback_with_port_is_allowed() {
    assert_eq!(status_with_host(&format!("[::1]:{PORT}")).await.status, StatusCode::OK);
}

#[tokio::test]
async fn host_with_other_port_is_403_host_not_allowed() {
    let r = status_with_host(&format!("127.0.0.1:{}", PORT + 1)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.error_code(), "host_not_allowed");
}

#[tokio::test]
async fn host_without_port_is_403() {
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        let r = status_with_host(host).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "Host: {host}");
        assert_eq!(r.error_code(), "host_not_allowed");
    }
}

#[tokio::test]
async fn host_foreign_domain_is_403_even_with_valid_token() {
    let r = status_with_host(&format!("evil.example:{PORT}")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.error_code(), "host_not_allowed");
}

#[tokio::test]
async fn host_lookalikes_are_403() {
    for host in [
        format!("127.0.0.1.evil.example:{PORT}"),
        format!("localhost.evil.example:{PORT}"),
        format!("evil.example:{PORT}@127.0.0.1"),
    ] {
        let r = status_with_host(&host).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "Host: {host}");
    }
}

#[tokio::test]
async fn missing_host_is_403() {
    let (gate, token) = open_gate();
    let r = send(&api_app(&gate), get(STATUS).without("host").bearer(&token)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.error_code(), "host_not_allowed");
}

// ---- Origin と CORS ----

#[tokio::test]
async fn any_origin_header_is_403_origin_not_allowed() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    for origin in ["http://evil.example", "null", &format!("http://127.0.0.1:{PORT}")] {
        let r = send(&app, get(STATUS).origin(origin).bearer(&token)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "Origin: {origin}");
        assert_eq!(r.error_code(), "origin_not_allowed", "Origin: {origin}");
    }
}

#[tokio::test]
async fn preflight_options_is_403_without_cors_headers() {
    let (gate, _t) = open_gate();
    let r = send(
        &api_app(&gate),
        req(Method::OPTIONS, STATUS)
            .origin("http://evil.example")
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization"),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.cors_header_names().is_empty(), "CORS ヘッダ: {:?}", r.cors_header_names());
}

#[tokio::test]
async fn api_responses_never_carry_cors_headers() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    let ok = send(&app, get(STATUS).bearer(&token)).await;
    let unauth = send(&app, get(STATUS)).await;
    let forbidden = send(&app, get(STATUS).origin("http://evil.example").bearer(&token)).await;
    let not_found = send(&app, get("/api/v1/nope").bearer(&token)).await;
    let disabled = send(&api_app(&ApiGate::closed()), get(STATUS).origin("http://evil.example")).await;
    for (label, r, code) in [
        ("200", ok, 200),
        ("401", unauth, 401),
        ("403", forbidden, 403),
        ("404", not_found, 404),
        ("404 無効時", disabled, 404),
    ] {
        assert_eq!(r.status.as_u16(), code, "{label}");
        assert!(r.cors_header_names().is_empty(), "{label}: {:?}", r.cors_header_names());
    }
}

// ---- 順序と体裁 ----

#[tokio::test]
async fn bad_host_without_token_is_403_not_401() {
    let (gate, _t) = open_gate();
    let r = send(&api_app(&gate), get(STATUS).host(&format!("evil.example:{PORT}"))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.error_code(), "host_not_allowed");
}

#[tokio::test]
async fn origin_with_wrong_token_is_403_not_401() {
    let (gate, _t) = open_gate();
    let r = send(
        &api_app(&gate),
        get(STATUS).origin("http://evil.example").bearer("fc_wrong"),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.error_code(), "origin_not_allowed");
}

#[tokio::test]
async fn unknown_path_without_token_is_401() {
    let (gate, _t) = open_gate();
    let app = api_app(&gate);
    for path in ["/api/v1/nope", "/api/nope", "/api", "/api/"] {
        let r = send(&app, get(path)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn unknown_path_with_token_is_404_not_found_json() {
    let (gate, token) = open_gate();
    let r = send(&api_app(&gate), get("/api/v1/nope").bearer(&token)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.error_code(), "not_found");
}

#[tokio::test]
async fn wrong_method_on_status_is_405_method_not_allowed() {
    let (gate, token) = open_gate();
    let r = send(&api_app(&gate), req(Method::POST, STATUS).bearer(&token)).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.error_code(), "method_not_allowed");
}

#[tokio::test]
async fn json_responses_set_no_store_and_nosniff() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    let responses = [
        ("200", send(&app, get(STATUS).bearer(&token)).await),
        ("401", send(&app, get(STATUS)).await),
        ("403", send(&app, get(STATUS).host("evil.example:1").bearer(&token)).await),
        ("404", send(&app, get("/api/v1/nope").bearer(&token)).await),
        ("405", send(&app, req(Method::POST, STATUS).bearer(&token)).await),
    ];
    for (label, r) in responses {
        let ct = r.header("content-type").unwrap_or_default().to_ascii_lowercase();
        assert_eq!(ct, "application/json; charset=utf-8", "{label}");
        assert_eq!(r.header("cache-control").as_deref(), Some("no-store"), "{label}");
        assert_eq!(r.header("x-content-type-options").as_deref(), Some("nosniff"), "{label}");
    }
}

#[tokio::test]
async fn error_body_has_error_code_and_message_keys_only() {
    let (gate, token) = open_gate();
    let app = api_app(&gate);
    let errors = [
        send(&app, get(STATUS)).await,
        send(&app, get(STATUS).host("evil.example:1").bearer(&token)).await,
        send(&app, get("/api/v1/nope").bearer(&token)).await,
        send(&app, req(Method::POST, STATUS).bearer(&token)).await,
    ];
    for r in errors {
        let j = r.json();
        let top: Vec<&str> = j.as_object().expect("オブジェクト").keys().map(String::as_str).collect();
        assert_eq!(top, ["error"], "{j}");
        let mut inner: Vec<&str> = j["error"].as_object().expect("error はオブジェクト").keys().map(String::as_str).collect();
        inner.sort();
        assert_eq!(inner, ["code", "message"], "{j}");
        assert!(j["error"]["code"].is_string());
        assert!(j["error"]["message"].is_string());
    }
}
