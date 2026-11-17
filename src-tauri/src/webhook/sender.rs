use super::delivery::Outcome;
use futures_util::StreamExt;
use std::{future::Future, time::Duration};
#[derive(Clone)]
pub struct OutboundRequest {
    url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl OutboundRequest {
    pub fn new(url: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            url: url.into(),
            headers,
            body,
        }
    }
    pub fn url_for_test(&self) -> &str {
        &self.url
    }
}
impl std::fmt::Debug for OutboundRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutboundRequest { [redacted] }")
    }
}
pub trait Sender: Clone + Send + Sync + 'static {
    fn send(&self, request: OutboundRequest) -> impl Future<Output = Outcome> + Send;
}
#[derive(Clone)]
pub struct ReqwestSender(reqwest::Client);
impl Default for ReqwestSender {
    fn default() -> Self {
        Self::new()
    }
}
impl ReqwestSender {
    pub fn new() -> Self {
        Self::with_timeouts(Duration::from_secs(5), Duration::from_secs(10))
    }
    pub fn with_timeouts(connect: Duration, total: Duration) -> Self {
        Self(
            reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(connect)
                .timeout(total)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("Webhook HTTP client"),
        )
    }
}
fn seconds(s: &str) -> Option<Duration> {
    s.parse::<f64>().ok().and_then(retry_seconds)
}
// 項目の最大寿命を超える待ちは意味がない。Duration の範囲を超える有限値も
// 無視せず上限へ丸め、早すぎる再送と巨大な ready_at の両方を防ぐ。
fn retry_seconds(n: f64) -> Option<Duration> {
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(n.min(600.0)).ok()
}

impl Sender for ReqwestSender {
    async fn send(&self, request: OutboundRequest) -> Outcome {
        let mut builder = self.0.post(&request.url);
        for (k, v) in &request.headers {
            builder = builder.header(k, v);
        }
        let response = match builder.body(request.body).send().await {
            Ok(r) => r,
            Err(e) => {
                let e = e.without_url();
                return if e.is_connect() {
                    Outcome::ConnectFailed
                } else if e.is_timeout() {
                    Outcome::TimedOutAfterSend
                } else {
                    Outcome::ReadFailed
                };
            }
        };
        let code = response.status().as_u16();
        let header_retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|s| s.to_str().ok())
            .and_then(seconds);
        let remaining_zero_reset_after = if response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|s| s.to_str().ok())
            == Some("0")
        {
            response
                .headers()
                .get("x-ratelimit-reset-after")
                .and_then(|s| s.to_str().ok())
                .and_then(seconds)
        } else {
            None
        };
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while bytes.len() < 65536 {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    bytes.extend_from_slice(&chunk[..chunk.len().min(65536 - bytes.len())])
                }
                Some(Err(e)) => {
                    let _ = e.without_url();
                    // 2xx で本文が切れたら配送は未確定。読み取り失敗として扱う(設計 §3.5)。
                    // それ以外はステータスを受け取れているので、ステータスで判定する。
                    if (200..300).contains(&code) {
                        return Outcome::ReadFailedWithRateLimit {
                            header_retry_after,
                            remaining_zero_reset_after,
                        };
                    }
                    return Outcome::Status {
                        code,
                        header_retry_after,
                        body_retry_after: None,
                        remaining_zero_reset_after,
                    };
                }
                None => break,
            }
        }
        let body_retry_after = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("retry_after").and_then(|n| n.as_f64()))
            .and_then(retry_seconds);
        Outcome::Status {
            code,
            header_retry_after,
            body_retry_after,
            remaining_zero_reset_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        integrations::{
            Integrations, MemorySecretStore, WebhookFormat, WebhookInput, WebhookTriggers,
        },
        model::ChatMessage,
        webhook::WebhookRuntime,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[derive(Clone)]
    struct Forward {
        calls: Arc<AtomicUsize>,
        url: String,
    }
    impl Sender for Forward {
        async fn send(&self, _: OutboundRequest) -> Outcome {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ReqwestSender::new()
                    .send(OutboundRequest::new(&self.url, vec![], vec![]))
                    .await
            } else {
                Outcome::Status {
                    code: 204,
                    header_retry_after: None,
                    body_retry_after: None,
                    remaining_zero_reset_after: None,
                }
            }
        }
    }

    // 外部レビューの review_rate.rs から取り込んだ回帰テスト。
    #[tokio::test]
    async fn truncated_2xx_must_preserve_discord_reset_after() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-After: 30\r\nConnection: close\r\n\r\nx").unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        it.save_webhook(WebhookInput {
            id: None,
            name: "test".into(),
            format: WebhookFormat::Discord,
            enabled: true,
            triggers: WebhookTriggers {
                donation: true,
                ..Default::default()
            },
            keywords: vec![],
            url: Some("https://discord.com/api/webhooks/id/token".into()),
        })
        .await
        .unwrap();
        let sender = Forward {
            calls: Arc::new(AtomicUsize::new(0)),
            url,
        };
        let rt = WebhookRuntime::new(it, sender.clone());
        rt.reload().await;
        for id in ["a", "b"] {
            let msg: ChatMessage = serde_json::from_value(serde_json::json!({"id":id,"platform":"youtube","channel":"test","author":{"id":"a","name":"A"},"kind":"superChat","timestampMs":1,"fragments":[{"type":"text","text":"hi"}]})).unwrap();
            rt.handle_message(&msg);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        server.join().unwrap();
        assert_eq!(rt.status()[0].uncertain, 1);
        assert_eq!(sender.calls.load(Ordering::SeqCst), 1, "Remaining=0 / Reset-After=30 must block the next item even if the 200 body is truncated");
        rt.shutdown();
    }

    #[tokio::test]
    async fn truncated_2xx_must_preserve_generic_retry_after() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nRetry-After: 4\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-After: 30\r\nConnection: close\r\n\r\nx").unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        it.save_webhook(WebhookInput {
            id: None,
            name: "test".into(),
            format: WebhookFormat::Generic,
            enabled: true,
            triggers: WebhookTriggers {
                donation: true,
                ..Default::default()
            },
            keywords: vec![],
            url: Some(url.clone()),
        })
        .await
        .unwrap();
        let sender = Forward {
            calls: Arc::new(AtomicUsize::new(0)),
            url,
        };
        let rt = WebhookRuntime::new(it, sender.clone());
        rt.reload().await;
        for id in ["a", "b"] {
            let msg: ChatMessage = serde_json::from_value(serde_json::json!({"id":id,"platform":"youtube","channel":"test","author":{"id":"a","name":"A"},"kind":"superChat","timestampMs":1,"fragments":[{"type":"text","text":"hi"}]})).unwrap();
            rt.handle_message(&msg);
        }
        // 通常の初回バックオフ(最大 2.4 秒)を超えても Retry-After が優先される。
        tokio::time::sleep(Duration::from_secs(3)).await;
        server.join().unwrap();
        assert_eq!(
            sender.calls.load(Ordering::SeqCst),
            1,
            "Retry-After must delay both the retry and the next item after a truncated 200 body"
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while rt.status()[0].delivered < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("generic delivery must retry after Retry-After, ignoring Discord bucket headers");
        assert_eq!(sender.calls.load(Ordering::SeqCst), 3);
        assert_eq!(rt.status()[0].uncertain, 0);
        rt.shutdown();
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }
    #[tokio::test]
    async fn response_reads_fractional_retry_and_bucket_headers() {
        let url = serve(axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [
                        ("retry-after", "2.5"),
                        ("x-ratelimit-remaining", "0"),
                        ("x-ratelimit-reset-after", "1.75"),
                    ],
                    r#"{"retry_after":4.25}"#,
                )
            }),
        ))
        .await;
        assert_eq!(
            ReqwestSender::new()
                .send(OutboundRequest::new(&url, vec![], vec![]))
                .await,
            Outcome::Status {
                code: 429,
                header_retry_after: Some(Duration::from_millis(2500)),
                body_retry_after: Some(Duration::from_millis(4250)),
                remaining_zero_reset_after: Some(Duration::from_millis(1750))
            }
        );
    }
    #[tokio::test]
    async fn response_stops_at_64k_without_waiting_for_unbounded_body() {
        let url = serve(axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                let mut data = br#"{"retry_after":3}"#.to_vec();
                data.resize(65536, b' ');
                let stream = futures_util::stream::once(async move {
                    Ok::<_, std::io::Error>(bytes::Bytes::from(data))
                })
                .chain(futures_util::stream::pending());
                axum::body::Body::from_stream(stream)
            }),
        ))
        .await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            ReqwestSender::new().send(OutboundRequest::new(&url, vec![], vec![])),
        )
        .await
        .expect("64 KiB で読み取りを止める");
        assert!(
            matches!(outcome, Outcome::Status { code: 200, body_retry_after: Some(d), .. } if d == Duration::from_secs(3))
        );
    }
    #[tokio::test]
    async fn regression_429_body_failure_preserves_headers() {
        let url = serve(axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                let stream = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{"))
                })
                .chain(futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Err(std::io::Error::other("broken body"))
                }));
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "12")],
                    axum::body::Body::from_stream(stream),
                )
            }),
        ))
        .await;
        assert_eq!(
            ReqwestSender::new()
                .send(OutboundRequest::new(&url, vec![], vec![]))
                .await,
            Outcome::Status {
                code: 429,
                header_retry_after: Some(Duration::from_secs(12)),
                body_retry_after: None,
                remaining_zero_reset_after: None
            }
        );
    }
    #[tokio::test]
    async fn regression_body_failure_keeps_received_status() {
        for code in [404u16, 200] {
            let url = serve(axum::Router::new().route(
                "/",
                axum::routing::post(move || async move {
                    let stream = futures_util::stream::once(async {
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"0123456789"))
                    })
                    .chain(futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(std::io::Error::other("broken body"))
                    }));
                    (
                        axum::http::StatusCode::from_u16(code).unwrap(),
                        axum::body::Body::from_stream(stream),
                    )
                }),
            ))
            .await;
            let outcome = ReqwestSender::new()
                .send(OutboundRequest::new(&url, vec![], vec![]))
                .await;
            if code == 200 {
                // 2xx で本文が切れたら配送は未確定。読み取り失敗の方針(設計 §3.5)に従う。
                use crate::{
                    integrations::WebhookFormat,
                    webhook::delivery::{classify, Decision},
                };
                assert_eq!(
                    outcome,
                    Outcome::ReadFailedWithRateLimit {
                        header_retry_after: None,
                        remaining_zero_reset_after: None,
                    }
                );
                assert_eq!(
                    classify(WebhookFormat::Discord, &outcome),
                    Decision::Drop { uncertain: true }
                );
                assert_eq!(
                    classify(WebhookFormat::Generic, &outcome),
                    Decision::Retry { wait: None }
                );
            } else {
                assert_eq!(
                    outcome,
                    Outcome::Status {
                        code,
                        header_retry_after: None,
                        body_retry_after: None,
                        remaining_zero_reset_after: None
                    },
                    "code={code}"
                );
            }
        }
    }
    #[tokio::test]
    async fn regression_sender_ignores_environment_proxy() {
        const CHILD: &str = "FC_WEBHOOK_PROXY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "webhook::sender::tests::regression_sender_ignores_environment_proxy",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HTTP_PROXY", "http://127.0.0.1:1")
                .env("ALL_PROXY", "http://127.0.0.1:1")
                .env("NO_PROXY", "")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let url = serve(axum::Router::new().route(
            "/",
            axum::routing::post(|| async { axum::http::StatusCode::NO_CONTENT }),
        ))
        .await;
        assert!(matches!(
            ReqwestSender::new()
                .send(OutboundRequest::new(&url, vec![], vec![]))
                .await,
            Outcome::Status { code: 204, .. }
        ));
    }
    #[tokio::test]
    async fn regression_oversized_retry_numbers_are_capped_not_ignored() {
        let url = serve(axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [
                        ("retry-after", "1e100"),
                        ("x-ratelimit-remaining", "0"),
                        ("x-ratelimit-reset-after", "1e100"),
                    ],
                    r#"{"retry_after":1e100}"#,
                )
            }),
        ))
        .await;
        assert_eq!(
            ReqwestSender::new()
                .send(OutboundRequest::new(&url, vec![], vec![]))
                .await,
            Outcome::Status {
                code: 429,
                header_retry_after: Some(Duration::from_secs(600)),
                body_retry_after: Some(Duration::from_secs(600)),
                remaining_zero_reset_after: Some(Duration::from_secs(600))
            }
        );
        for invalid in ["-1", "NaN", "inf"] {
            assert_eq!(seconds(invalid), None);
        }
    }
}
