//! PR1 受け入れテスト: 既存 OBS ルートと API ルータの境界(設計書 §5.3 / §2.5)。
//! `Bus::obs_app` に API ルータを載せた合成アプリ。テンプレ置き場は内蔵 `default` だけの一時ディレクトリ。

use crate::common::*;
use axum::http::{Method, StatusCode};
use axum::Router;
use fast_comment_lib::api::{self, ApiDeps, ApiGate};
use fast_comment_lib::bus::Bus;
use fast_comment_lib::model::{Author, ChatMessage, Fragment, MessageKind, Platform, Roles};
use fast_comment_lib::stats::{StatsSnapshot, TimerSnapshot};
use futures_util::StreamExt;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Fixture {
    bus: Bus,
    app: Router,
    _dirs: TempDir,
}

fn make_bus(port: u16) -> Bus {
    let (stats_tx, _) = watch::channel(StatsSnapshot::default());
    let (timer_tx, _) = watch::channel(TimerSnapshot::default());
    Bus::new(port, stats_tx, timer_tx)
}

/// `extra` に API ルータを載せるか。`gate` が閉なら API 無効の合成アプリ。
fn fixture(port: u16, gate: Option<&ApiGate>) -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let templates = dirs.path().join("templates");
    std::fs::create_dir_all(templates.join("default")).unwrap();
    std::fs::write(templates.join("default/index.html"), "<html><head></head><body>default</body></html>").unwrap();
    let custom = dirs.path().join("custom");
    std::fs::create_dir_all(&custom).unwrap();
    let skin = dirs.path().join("skin");
    std::fs::create_dir_all(&skin).unwrap();
    let bus = make_bus(port);
    let extra = gate.map(|g| api::router(ApiDeps::new(g.clone(), CancellationToken::new()), port));
    let app = bus.obs_app(templates, custom, skin, extra);
    Fixture { bus, app, _dirs: dirs }
}

fn open_fixture() -> (Fixture, String) {
    let (gate, token) = open_gate();
    (fixture(PORT, Some(&gate)), token)
}

fn sample_message(id: &str) -> ChatMessage {
    ChatMessage {
        route: Default::default(),
        sound: None,
        listener: None,
        id: id.to_string(),
        native_id: false,
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        author: Author {
            nickname: None,
            avatar_url: None,
            id: "u1".to_string(),
            name: "viewer".to_string(),
            display_color: None,
            badges: vec![],
            roles: Roles::default(),
        },
        fragments: vec![Fragment::text("hello")],
        kind: MessageKind::Normal,
        amount: None,
        timestamp_ms: 1_731_600_000_000,
        raw: None,
        skip_tts: false,
        injected_test: false,
    }
}

#[tokio::test]
async fn legacy_template_index_is_served_with_api_enabled() {
    let (fx, _t) = open_fixture();
    let r = send(&fx.app, get("/?template=default")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&r.body).contains("default"));
}

#[tokio::test]
async fn legacy_routes_keep_permissive_cors() {
    let (fx, _t) = open_fixture();
    let r = send(&fx.app, get("/?template=default").origin("http://example.com")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("access-control-allow-origin").as_deref(), Some("*"));
}

#[tokio::test]
async fn legacy_routes_do_not_check_host() {
    let (fx, _t) = open_fixture();
    let r = send(&fx.app, get("/?template=default").host("foo.example")).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn legacy_ws_works_without_token_with_api_enabled() {
    let (gate, _t) = open_gate();
    // 実ソケットが要る(WS)。bind 先のポートで API ルータも作る。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let fx = fixture(port, Some(&gate));
    let bus = fx.bus.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, fx.app.clone()).await;
        drop(fx);
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("トークン無しで /ws に接続できる");
    let wanted = "ws-legacy-1";
    let received = tokio::time::timeout(Duration::from_secs(5), async {
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                _ = tick.tick() => bus.publish(sample_message(wanted)),
                frame = ws.next() => {
                    let Some(Ok(frame)) = frame else { return None };
                    let Ok(text) = frame.into_text() else { continue };
                    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                    if let Some(items) = json.as_array() {
                        if items.iter().any(|m| m["id"] == wanted) {
                            return Some(json);
                        }
                    }
                }
            }
        }
    })
    .await
    .expect("5 秒以内にコメントが届く");
    assert!(received.is_some_and(|v| v.is_array()));
    server.abort();
}

#[tokio::test]
async fn legacy_bad_template_id_still_400_and_missing_custom_still_404() {
    let (fx, _t) = open_fixture();
    let bad = send(&fx.app, get("/?template=bad.id")).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let missing = send(&fx.app, get("/?template=custom:nope")).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_prefix_boundary() {
    let (fx, _t) = open_fixture();
    for path in ["/apix", "/api-docs"] {
        let r = send(&fx.app, get(path)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path} は従来の 404");
        let ct = r.header("content-type").unwrap_or_default().to_ascii_lowercase();
        assert!(!ct.starts_with("application/json"), "{path} が API の JSON を返した");
        assert!(r.header("www-authenticate").is_none(), "{path} は API の認証に入らない");
    }
    for path in ["/api", "/api/"] {
        let r = send(&fx.app, get(path)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path} は API の受け皿(トークンなし)");
    }
}

#[tokio::test]
async fn composed_app_returns_404_for_api_paths_when_disabled() {
    let fx = fixture(PORT, Some(&ApiGate::closed()));
    for path in ["/api/v1/status", "/api", "/api/", "/api/v1/nope"] {
        let r = send(&fx.app, get(path)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
        assert!(r.body.is_empty(), "{path} の本文は空");
    }
    let post = send(&fx.app, req(Method::POST, "/api/v1/status")).await;
    assert_eq!(post.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn composed_app_without_extra_router_behaves_as_before() {
    let fx = fixture(PORT, None);
    let r = send(&fx.app, get("/api/v1/status")).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let legacy = send(&fx.app, get("/?template=default")).await;
    assert_eq!(legacy.status, StatusCode::OK);
}
