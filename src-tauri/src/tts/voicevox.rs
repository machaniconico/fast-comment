//! VOICEVOX TTS バックエンド。
//!
//! 既定 `http://127.0.0.1:50021`。手順:
//!  1. `POST /audio_query?text=...&speaker=N` で音声合成クエリ(JSON)取得。
//!  2. `POST /synthesis?speaker=N`(body=クエリJSON)で wav バイトを取得。
//!
//! このモジュールは合成した wav バイトを返すだけで emit しない。base64 化と UI への
//! 送出は `TtsDispatcher` / ワーカー側が行う(「重い合成は Rust、再生は薄く UI」方針)。

use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

use super::params::{apply_to_audio_query, ResolvedVoiceParams};

/// 合成リクエスト全体のタイムアウト(エンジン stall 時に永久待ちを防ぐ)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// probe(バージョン / 話者一覧)のタイムアウト。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// エンジンの話者スタイル 1 件(1 スタイル 1 行)。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtsEngineSpeaker {
    pub id: u32,
    pub name: String,
    pub style: String,
}

/// probe の結果。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtsEngineProbe {
    pub version: String,
    pub speakers: Vec<TtsEngineSpeaker>,
}

/// VOICEVOX 互換 HTTP クライアント(AppHandle を持たない)。
#[derive(Clone)]
pub struct VoicevoxClient {
    base_url: String,
    http: Client,
}

impl VoicevoxClient {
    pub fn new(base_url: &str) -> Self {
        VoicevoxClient {
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            http: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    /// 合成して wav バイトを返す。リスナー指定話者が 4xx で拒否されたら全体話者で 1 回だけ再試行する。
    pub async fn synthesize(
        &self,
        text: &str,
        global_speaker: u32,
        listener_speaker: Option<u32>,
        params: &ResolvedVoiceParams,
    ) -> anyhow::Result<Vec<u8>> {
        let speaker = selected_speaker(global_speaker, listener_speaker);
        match self.synthesize_once(text, speaker, params).await {
            Ok(wav) => Ok(wav),
            Err(SynthError::Client(status)) if speaker != global_speaker => {
                tracing::warn!(
                    "VOICEVOX: 話者 {speaker} が {status} で拒否、全体話者 {global_speaker} で再試行"
                );
                self.synthesize_once(text, global_speaker, params)
                    .await
                    .map_err(anyhow::Error::from)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn synthesize_once(
        &self,
        text: &str,
        speaker: u32,
        params: &ResolvedVoiceParams,
    ) -> Result<Vec<u8>, SynthError> {
        let speaker = speaker.to_string();

        // 1) audio_query
        let resp = self
            .http
            .post(format!("{}/audio_query", self.base_url))
            .query(&[("text", text), ("speaker", speaker.as_str())])
            .send()
            .await?;
        let mut query: Value = check(resp)?.json().await?;
        apply_to_audio_query(&mut query, params);

        // 2) synthesis(wav バイト)
        let resp = self
            .http
            .post(format!("{}/synthesis", self.base_url))
            .query(&[("speaker", speaker.as_str())])
            .json(&query)
            .send()
            .await?;
        Ok(check(resp)?.bytes().await?.to_vec())
    }

    /// `/version` と `/speakers` を取得する。欠落は寛容に None / スキップで劣化させる。
    pub async fn probe(&self) -> anyhow::Result<TtsEngineProbe> {
        let version: Value = self
            .http
            .get(format!("{}/version", self.base_url))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let version = version.as_str().unwrap_or_default().to_string();

        let speakers: Value = self
            .http
            .get(format!("{}/speakers", self.base_url))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(TtsEngineProbe {
            version,
            speakers: flatten_speakers(&speakers),
        })
    }
}

#[derive(Debug, thiserror::Error)]
enum SynthError {
    #[error("HTTP {0}")]
    Client(reqwest::StatusCode),
    #[error(transparent)]
    Other(#[from] reqwest::Error),
}

fn check(resp: reqwest::Response) -> Result<reqwest::Response, SynthError> {
    let status = resp.status();
    if status.is_client_error() {
        Err(SynthError::Client(status))
    } else {
        Ok(resp.error_for_status()?)
    }
}

/// `/speakers` を 1 スタイル 1 行へ平坦化。type が talk 以外(sing 等)は除外、欠落は飛ばす。
fn flatten_speakers(v: &Value) -> Vec<TtsEngineSpeaker> {
    let mut out = Vec::new();
    for sp in v.as_array().into_iter().flatten() {
        let name = sp.get("name").and_then(Value::as_str).unwrap_or_default();
        for st in sp.get("styles").and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = st.get("type").and_then(Value::as_str) {
                if t != "talk" {
                    continue;
                }
            }
            let Some(id) = st
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
            else {
                continue;
            };
            out.push(TtsEngineSpeaker {
                id,
                name: name.to_string(),
                style: st
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }
    out
}

fn selected_speaker(global: u32, listener: Option<u32>) -> u32 {
    listener.unwrap_or(global)
}

/// 依存追加を避けるための最小 base64 エンコーダ(標準アルファベット)。
pub(crate) mod base64_encode {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// バイト列を base64 文字列へ変換する。
    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;

            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::encode;

        #[test]
        fn encodes_rfc4648_vectors_with_standard_alphabet() {
            let cases = [
                (b"".as_slice(), ""),
                (b"f".as_slice(), "Zg=="),
                (b"fo".as_slice(), "Zm8="),
                (b"foo".as_slice(), "Zm9v"),
                (b"foob".as_slice(), "Zm9vYg=="),
                (b"fooba".as_slice(), "Zm9vYmE="),
                (b"foobar".as_slice(), "Zm9vYmFy"),
                (&[0xfb, 0xff, 0xff], "+///"),
            ];

            for (input, expected) in cases {
                assert_eq!(encode(input), expected);
            }
        }
    }
}

/// テスト用の偽 VOICEVOX エンジン(US-7 のワーカーテストからも使う)。
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::{
        body::Bytes,
        extract::{Query, State},
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use serde_json::{json, Value};

    #[derive(Default)]
    pub struct FakeState {
        /// audio_query で受けた (text, speaker)。
        pub queries: Vec<(String, String)>,
        /// synthesis で受けた (speaker, body)。
        pub syntheses: Vec<(String, Value)>,
        /// このスピーカーでの synthesis は 400 にする。
        pub reject_speakers: Vec<String>,
        pub audio_query: Value,
        pub speakers: Value,
        pub wav: Vec<u8>,
    }

    pub struct FakeEngine {
        pub url: String,
        pub state: Arc<Mutex<FakeState>>,
    }

    pub async fn spawn_fake_engine() -> FakeEngine {
        let state = Arc::new(Mutex::new(FakeState {
            audio_query: json!({
                "speedScale": 1.0, "pitchScale": 0.0, "intonationScale": 1.0,
                "volumeScale": 1.0, "prePhonemeLength": 0.1, "postPhonemeLength": 0.1,
                "accent_phrases": [{"moras": []}], "kana": "テスト"
            }),
            speakers: json!([]),
            wav: b"RIFFfake".to_vec(),
            ..Default::default()
        }));
        let app = Router::new()
            .route("/version", get(|| async { Json(json!("0.99.0")) }))
            .route(
                "/speakers",
                get(|State(s): State<Arc<Mutex<FakeState>>>| async move {
                    Json(s.lock().unwrap().speakers.clone())
                }),
            )
            .route(
                "/audio_query",
                post(
                    |State(s): State<Arc<Mutex<FakeState>>>,
                     Query(q): Query<HashMap<String, String>>| async move {
                        let mut g = s.lock().unwrap();
                        g.queries.push((
                            q.get("text").cloned().unwrap_or_default(),
                            q.get("speaker").cloned().unwrap_or_default(),
                        ));
                        Json(g.audio_query.clone())
                    },
                ),
            )
            .route(
                "/synthesis",
                post(
                    |State(s): State<Arc<Mutex<FakeState>>>,
                     Query(q): Query<HashMap<String, String>>,
                     body: Bytes| async move {
                        let speaker = q.get("speaker").cloned().unwrap_or_default();
                        let mut g = s.lock().unwrap();
                        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        g.syntheses.push((speaker.clone(), body));
                        if g.reject_speakers.contains(&speaker) {
                            return (StatusCode::BAD_REQUEST, Vec::new());
                        }
                        (StatusCode::OK, g.wav.clone())
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeEngine { url, state }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::spawn_fake_engine;
    use super::*;
    use serde_json::json;

    fn empty_params() -> ResolvedVoiceParams {
        ResolvedVoiceParams {
            speed: None,
            speed_factor: 1.0,
            max_speed: 2.0,
            pitch: None,
            intonation: None,
            volume: None,
            pre: None,
            post: None,
        }
    }

    #[test]
    fn listener_voice_selects_speaker_for_both_voicevox_requests() {
        assert_eq!(super::selected_speaker(3, None), 3);
        assert_eq!(super::selected_speaker(3, Some(7)), 7);
        assert_eq!(super::selected_speaker(3, Some(0)), 0);
    }

    #[tokio::test]
    async fn synthesize_sends_overridden_params_to_synthesis() {
        let fake = spawn_fake_engine().await;
        let client = VoicevoxClient::new(&fake.url);
        let mut p = empty_params();
        p.speed = Some(1.5);
        p.pitch = Some(0.1);
        p.volume = Some(0.5);
        let wav = client.synthesize("こんにちは", 3, Some(8), &p).await.unwrap();
        assert_eq!(wav, b"RIFFfake");
        let g = fake.state.lock().unwrap();
        assert_eq!(g.queries, vec![("こんにちは".to_string(), "8".to_string())]);
        assert_eq!(g.syntheses.len(), 1);
        let (speaker, body) = &g.syntheses[0];
        assert_eq!(speaker, "8");
        assert_eq!(body["speedScale"].as_f64().unwrap() as f32, 1.5);
        assert_eq!(body["pitchScale"].as_f64().unwrap() as f32, 0.1);
        assert_eq!(body["volumeScale"].as_f64().unwrap() as f32, 0.5);
        assert_eq!(body["intonationScale"], json!(1.0));
        assert_eq!(body["kana"], json!("テスト"));
    }

    #[tokio::test]
    async fn synthesize_without_params_forwards_audio_query_verbatim() {
        let fake = spawn_fake_engine().await;
        let client = VoicevoxClient::new(&format!("{}/", fake.url));
        client.synthesize("a", 1, None, &empty_params()).await.unwrap();
        let g = fake.state.lock().unwrap();
        assert_eq!(g.syntheses[0].0, "1");
        assert_eq!(g.syntheses[0].1, g.audio_query);
    }

    #[tokio::test]
    async fn listener_speaker_4xx_retries_with_global_speaker() {
        let fake = spawn_fake_engine().await;
        fake.state.lock().unwrap().reject_speakers.push("99".into());
        let client = VoicevoxClient::new(&fake.url);
        let wav = client
            .synthesize("a", 2, Some(99), &empty_params())
            .await
            .unwrap();
        assert_eq!(wav, b"RIFFfake");
        let g = fake.state.lock().unwrap();
        let speakers: Vec<&str> = g.syntheses.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(speakers, vec!["99", "2"]);
    }

    #[tokio::test]
    async fn global_speaker_4xx_is_an_error_without_retry() {
        let fake = spawn_fake_engine().await;
        fake.state.lock().unwrap().reject_speakers.push("2".into());
        let client = VoicevoxClient::new(&fake.url);
        assert!(client.synthesize("a", 2, None, &empty_params()).await.is_err());
        assert_eq!(fake.state.lock().unwrap().syntheses.len(), 1);
    }

    #[tokio::test]
    async fn probe_returns_version_and_flattened_talk_styles() {
        let fake = spawn_fake_engine().await;
        fake.state.lock().unwrap().speakers = json!([
            {"name": "A", "speaker_uuid": "u1", "styles": [
                {"id": 1, "name": "ノーマル", "type": "talk"},
                {"id": 2, "name": "あまあま"}
            ]},
            {"name": "B", "styles": [{"id": 3, "name": "ノーマル", "type": "talk"}]}
        ]);
        let probe = VoicevoxClient::new(&fake.url).probe().await.unwrap();
        assert_eq!(probe.version, "0.99.0");
        let got: Vec<(u32, &str, &str)> = probe
            .speakers
            .iter()
            .map(|s| (s.id, s.name.as_str(), s.style.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![(1, "A", "ノーマル"), (2, "A", "あまあま"), (3, "B", "ノーマル")]
        );
    }

    #[tokio::test]
    async fn probe_tolerates_missing_fields_and_non_talk_styles() {
        let fake = spawn_fake_engine().await;
        fake.state.lock().unwrap().speakers = json!([
            {"styles": [
                {"id": 5, "type": "sing"},
                {"id": 6, "type": "frame_decode"},
                {"name": "idなし", "type": "talk"},
                {"id": 7, "type": "talk"}
            ]},
            {"name": "名前だけ"},
            "garbage",
            {"name": "C", "styles": "bad"}
        ]);
        let probe = VoicevoxClient::new(&fake.url).probe().await.unwrap();
        assert_eq!(
            probe.speakers,
            vec![TtsEngineSpeaker {
                id: 7,
                name: String::new(),
                style: String::new()
            }]
        );
    }

    #[tokio::test]
    async fn probe_reports_connection_refused_as_err() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let r = VoicevoxClient::new(&format!("http://127.0.0.1:{port}"))
            .probe()
            .await;
        assert!(r.is_err());
    }
}
