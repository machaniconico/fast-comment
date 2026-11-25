use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::{header::LOCATION, redirect::Policy, Response, StatusCode};
use url::{Host, Url};

const MAX_REDIRECTS: usize = 3;
const MAX_BYTES: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_URL: &str = "https://で始まる有効なURLを指定してください";
const DNS_ERROR: &str = "辞書URLの名前を解決できませんでした";
const FORBIDDEN_IP: &str = "辞書URLの接続先は公開IPアドレスに限ります";
const REQUEST_ERROR: &str = "辞書URLに接続できませんでした";
const REDIRECT_ERROR: &str = "辞書URLのリダイレクト先が不正です";
const REDIRECT_LIMIT: &str = "辞書URLのリダイレクトは3回までです";
const BODY_ERROR: &str = "辞書URLの応答を読み込めませんでした";
const BODY_LIMIT: &str = "辞書URLの本文は256 KiBまでです";
const TIMEOUT_ERROR: &str = "辞書URLの取得がタイムアウトしました";

pub(crate) async fn fetch(raw: &str) -> Result<Vec<u8>, String> {
    fetch_with(raw, TIMEOUT, request).await
}

// タイムアウトは DNS・全リダイレクト・本文を通して1つ。
async fn fetch_with(
    raw: &str,
    timeout: Duration,
    send: impl AsyncFn(Url, SocketAddr) -> Result<Response, String>,
) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, async {
        let raw = raw.trim();
        if !raw
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        {
            return Err(INVALID_URL.into());
        }
        let mut url = Url::parse(raw).map_err(|_| INVALID_URL)?;
        for redirects in 0..=MAX_REDIRECTS {
            let address = resolve(&url).await?;
            let response = send(url.clone(), address).await?;
            let status = response.status();
            if matches!(
                status,
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                if redirects == MAX_REDIRECTS {
                    return Err(REDIRECT_LIMIT.into());
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(REDIRECT_ERROR)?;
                url = url.join(location).map_err(|_| REDIRECT_ERROR)?;
            } else if status.is_success() {
                return read_body(response).await;
            } else {
                return Err(format!("辞書URLの応答がHTTP {}でした", status.as_u16()));
            }
        }
        unreachable!("redirect limit checked in loop")
    })
    .await
    .map_err(|_| TIMEOUT_ERROR.into())
    .and_then(|result| result)
}

async fn resolve(url: &Url) -> Result<SocketAddr, String> {
    let port = url.port_or_known_default().ok_or(INVALID_URL)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(INVALID_URL.into());
    }
    match url.host().ok_or(INVALID_URL)? {
        Host::Domain(domain) => select_public_address(
            tokio::net::lookup_host((domain, port))
                .await
                .map_err(|_| DNS_ERROR)?,
        ),
        Host::Ipv4(ip) => select_public_address([SocketAddr::new(ip.into(), port)]),
        Host::Ipv6(ip) => select_public_address([SocketAddr::new(ip.into(), port)]),
    }
}

fn select_public_address(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<SocketAddr, String> {
    let mut addresses = addresses.into_iter();
    let first = addresses.next().ok_or(DNS_ERROR)?;
    if !is_public_ip(first.ip()) || addresses.any(|addr| !is_public_ip(addr.ip())) {
        return Err(FORBIDDEN_IP.into());
    }
    Ok(first)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => is_public_v4(ip),
            None => is_public_v6(ip),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    !matches!(
        ip.octets(),
        [0 | 10 | 127 | 224..=255, ..]
            | [100, 64..=127, ..]
            | [169, 254, ..]
            | [172, 16..=31, ..]
            | [192, 0, 0 | 2, _]
            | [192, 88, 99, _]
            | [192, 168, ..]
            | [198, 18 | 19, ..]
            | [198, 51, 100, _]
            | [203, 0, 113, _]
    )
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // グローバルユニキャストの割当範囲から特殊用途・文書用・6to4を除く。
    matches!(ip.segments()[0], 0x2000..=0x3fff)
        && !matches!(
            ip.segments(),
            [0x2001, 0..=0x01ff | 0x0db8, ..] | [0x2002, ..] | [0x3fff, 0..=0x0fff, ..]
        )
}

async fn request(url: Url, address: SocketAddr) -> Result<Response, String> {
    // プロキシや自動リダイレクトを使うと、検査済みのアドレス以外へ接続し得る。
    let mut client = reqwest::Client::builder()
        .no_proxy()
        .https_only(true)
        // HTTP/2 のフレームログに :path (クエリを含む) が出るのを避ける。
        .http1_only()
        .no_gzip()
        .redirect(Policy::none())
        .retry(reqwest::retry::never());
    if let Some(domain) = url.domain() {
        // ホスト名を保持したまま TLS 証明書を検証し、接続先だけを固定する。
        client = client.resolve(domain, address);
    }
    client
        .build()
        .map_err(|_| REQUEST_ERROR)?
        .get(url)
        .send()
        .await
        .map_err(|_| REQUEST_ERROR.into())
}

async fn read_body(mut response: Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        return Err(BODY_LIMIT.into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BODY_ERROR)? {
        if chunk.len() > MAX_BYTES - body.len() {
            return Err(BODY_LIMIT.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Response as HttpResponse},
        Router,
    };
    use bytes::Bytes;
    use futures_util::stream;
    use tokio::net::TcpListener;

    const PUBLIC_URL: &str = "https://8.8.8.8/?token=private-query";

    #[test]
    fn public_ip_filter_covers_ipv4_ipv6_and_mapped_addresses() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.31.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fdff::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.1.1",
            "::ffff:0.0.0.0",
        ] {
            assert!(!is_public_ip(address.parse().unwrap()), "{address}");
        }
        for address in [
            "8.8.8.8",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn any_private_dns_answer_rejects_the_name() {
        let public = "8.8.8.8:443".parse().unwrap();
        let private = "10.0.0.1:443".parse().unwrap();
        assert_eq!(select_public_address([public]).unwrap(), public);
        assert_eq!(
            select_public_address([public, private]).unwrap_err(),
            FORBIDDEN_IP
        );
        assert_eq!(
            select_public_address([private, public]).unwrap_err(),
            FORBIDDEN_IP
        );
    }

    #[tokio::test]
    async fn http_private_literals_and_private_dns_fail_before_sending() {
        async fn never_send(_: Url, _: SocketAddr) -> Result<Response, String> {
            panic!("拒否する宛先への接続は開始しない");
        }
        for (url, error) in [
            ("http://8.8.8.8/?token=private-query", INVALID_URL),
            ("https:8.8.8.8", INVALID_URL),
            ("https://127.0.0.1/?token=private-query", FORBIDDEN_IP),
            ("https://[::ffff:127.0.0.1]/", FORBIDDEN_IP),
            ("https://localhost/?token=private-query", FORBIDDEN_IP),
        ] {
            assert_eq!(
                fetch_with(url, TIMEOUT, never_send).await.unwrap_err(),
                error
            );
        }
    }

    // テストだけ接続をローカルに差し替える。URL と IP の検証は本番の経路を通す。
    async fn fetch_mock(
        respond: impl Fn(usize) -> HttpResponse<Body> + Send + Sync + 'static,
    ) -> (Result<Vec<u8>, String>, usize) {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::clone(&calls);
        let respond = Arc::new(respond);
        let router = Router::new().fallback(move || {
            let respond = Arc::clone(&respond);
            let index = requests.fetch_add(1, Ordering::Relaxed);
            async move { respond(index) }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .unwrap();
        let result = fetch_with(PUBLIC_URL, TIMEOUT, async |_, address| {
            assert_eq!(address, "8.8.8.8:443".parse().unwrap());
            client
                .get(&local)
                .send()
                .await
                .map_err(|_| REQUEST_ERROR.into())
        })
        .await;
        server.abort();
        (result, calls.load(Ordering::Relaxed))
    }

    fn redirect(location: &str) -> HttpResponse<Body> {
        HttpResponse::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, location)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn private_redirect_is_rejected_before_the_next_request() {
        let (result, calls) =
            fetch_mock(|_| redirect("https://localhost/?token=redirect-query")).await;
        assert_eq!(result.unwrap_err(), FORBIDDEN_IP);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn allows_three_redirects_then_accepts_json_with_any_content_type() {
        let (result, calls) = fetch_mock(|index| {
            if index < 3 {
                redirect("/next?token=redirect-query")
            } else {
                HttpResponse::builder()
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(r#"[{"from":"X","to":"エックス"}]"#))
                    .unwrap()
            }
        })
        .await;
        assert!(crate::tts_dictionary::parse(&result.unwrap()).is_ok());
        assert_eq!(calls, 4);
    }

    #[tokio::test]
    async fn rejects_the_fourth_redirect() {
        let (result, calls) = fetch_mock(|_| redirect("/next?token=redirect-query")).await;
        assert_eq!(result.unwrap_err(), REDIRECT_LIMIT);
        assert_eq!(calls, 4);
    }

    #[tokio::test]
    async fn limits_streamed_body_with_no_content_length() {
        fn streamed_body(size: usize) -> HttpResponse<Body> {
            let chunks = [vec![b'x'; MAX_BYTES / 2], vec![b'x'; size - MAX_BYTES / 2]];
            let stream =
                stream::iter(chunks.map(|chunk| Ok::<_, std::io::Error>(Bytes::from(chunk))));
            HttpResponse::new(Body::from_stream(stream))
        }
        let (exact, _) = fetch_mock(|_| streamed_body(MAX_BYTES)).await;
        assert_eq!(exact.unwrap().len(), MAX_BYTES);
        let (oversized, _) = fetch_mock(|_| streamed_body(MAX_BYTES + 1)).await;
        assert_eq!(oversized.unwrap_err(), BODY_LIMIT);
    }

    #[tokio::test(start_paused = true)]
    async fn overall_deadline_interrupts_a_stalled_request() {
        let start = tokio::time::Instant::now();
        let error = fetch_with(PUBLIC_URL, TIMEOUT, async |_, _| {
            std::future::pending().await
        })
        .await
        .unwrap_err();
        assert_eq!(error, TIMEOUT_ERROR);
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn errors_do_not_echo_request_or_redirect_queries() {
        let (result, _) = fetch_mock(|_| redirect("http://8.8.8.8/?token=redirect-query")).await;
        let error = result.unwrap_err();
        assert!(!error.contains("private-query"));
        assert!(!error.contains("redirect-query"));
        assert_eq!(error, INVALID_URL);
    }

    #[tokio::test]
    async fn request_uses_pinned_address_and_sanitizes_tls_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = Url::parse(&format!(
            "https://dictionary.invalid:{}/?token=private-query",
            address.port()
        ))
        .unwrap();
        // TLS を話さないモックに接続しても、生の reqwest エラーは返さない。
        let (result, ()) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(request(url, address), async {
                let (socket, _) = listener.accept().await.unwrap();
                drop(socket);
            })
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap_err(), REQUEST_ERROR);
    }
}
