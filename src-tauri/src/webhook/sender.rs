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
                .connect_timeout(connect)
                .timeout(total)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("Webhook HTTP client"),
        )
    }
}
fn seconds(s: &str) -> Option<Duration> {
    s.parse::<f64>()
        .ok()
        .and_then(|n| Duration::try_from_secs_f64(n).ok())
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
                    return Outcome::ReadFailed;
                }
                None => break,
            }
        }
        let body_retry_after = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("retry_after").and_then(|n| n.as_f64()))
            .and_then(|n| Duration::try_from_secs_f64(n).ok());
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
}
