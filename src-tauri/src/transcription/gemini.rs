//! Gemini transcription provider implementing the common contract.
//!
//! This is the normalized, fully-testable Gemini client: request building,
//! response parsing and HTTP-status → error mapping are pure functions covered
//! by unit tests against a local mock server. The API key is passed in by the
//! caller (fetched from the keyring), never read from settings here.
//!
//! The legacy `gemini-api` model id remains supported by
//! `managers::transcription`; new explicit cloud targets use this provider.
//! Both paths read the same keyring credential and only the selected path runs.

use std::io::Cursor;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use hound::{SampleFormat, WavSpec, WavWriter};
use log::{debug, warn};
use serde::{Deserialize, Serialize};

use super::provider::TranscriptionProvider;
use super::types::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionError,
    TranscriptionLatency, TranscriptionModelDescriptor, TranscriptionRequest, TranscriptionResult,
};

pub const PROVIDER_ID: &str = "gemini";
pub const DEFAULT_MODEL: &str = "gemini-2.5-flash";

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const TRANSCRIBE_PROMPT: &str =
    "Transcribe this audio. Return only the transcript text, nothing else.";

/// Gemini batch transcription provider.
pub struct GeminiProvider {
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self::build(
            DEFAULT_BASE_URL.to_string(),
            DEFAULT_MODEL.to_string(),
            DEFAULT_TIMEOUT,
        )
    }

    /// Build a provider for the model selected in settings instead of silently
    /// replacing it with the default catalog model.
    pub fn with_model(model: impl Into<String>) -> Self {
        Self::build(DEFAULT_BASE_URL.to_string(), model.into(), DEFAULT_TIMEOUT)
    }

    fn build(base_url: String, model: String, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self {
            base_url,
            model,
            http,
        }
    }

    #[cfg(test)]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::build(base_url.into(), DEFAULT_MODEL.to_string(), DEFAULT_TIMEOUT)
    }

    #[cfg(test)]
    pub fn with_base_url_and_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self::build(base_url.into(), DEFAULT_MODEL.to_string(), timeout)
    }
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        id: PROVIDER_ID.to_string(),
        label: "Gemini".to_string(),
        kind: ProviderKind::Cloud,
        models: vec![TranscriptionModelDescriptor {
            id: DEFAULT_MODEL.to_string(),
            label: "Gemini 2.5 Flash".to_string(),
        }],
        capabilities: ProviderCapabilities {
            batch: true,
            realtime: false,
            supported_languages: vec![],
            supports_word_timestamps: false,
            sends_audio_off_device: true,
        },
        requires_credential: true,
        privacy_url: Some("https://ai.google.dev/gemini-api/terms".to_string()),
        pricing_url: Some("https://ai.google.dev/pricing".to_string()),
        cost_text: None,
        retention_text: Some("Retention follows your Google account and provider policy.".into()),
        consent_version: 1,
        beta: false,
    }
}

#[derive(Serialize)]
struct InlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<InlineData>,
}

#[derive(Serialize)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Serialize)]
struct GenerateContentRequest {
    contents: Vec<Content>,
}

#[derive(Deserialize)]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
}

#[derive(Deserialize)]
struct CandidateContent {
    parts: Option<Vec<ResponsePart>>,
}

#[derive(Deserialize)]
struct ResponsePart {
    text: Option<String>,
}

fn encode_wav(samples: &[f32]) -> Result<Vec<u8>, TranscriptionError> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut buffer = Vec::new();
    {
        let cursor = Cursor::new(&mut buffer);
        let mut writer = WavWriter::new(cursor, spec)
            .map_err(|_| TranscriptionError::InvalidConfiguration("wav header".to_string()))?;
        for sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            writer
                .write_sample((clamped * i16::MAX as f32) as i16)
                .map_err(|_| TranscriptionError::InvalidConfiguration("wav sample".to_string()))?;
        }
        writer
            .finalize()
            .map_err(|_| TranscriptionError::InvalidConfiguration("wav finalize".to_string()))?;
    }
    Ok(buffer)
}

fn build_request(audio_base64: String) -> GenerateContentRequest {
    GenerateContentRequest {
        contents: vec![Content {
            parts: vec![
                Part {
                    text: None,
                    inline_data: Some(InlineData {
                        mime_type: "audio/wav".to_string(),
                        data: audio_base64,
                    }),
                },
                Part {
                    text: Some(TRANSCRIBE_PROMPT.to_string()),
                    inline_data: None,
                },
            ],
        }],
    }
}

fn map_status_error(status: u16) -> TranscriptionError {
    match status {
        400 => TranscriptionError::InvalidConfiguration(format!(
            "provider rejected request ({status})"
        )),
        401 | 403 => TranscriptionError::Authentication,
        429 => TranscriptionError::RateLimited,
        500..=599 => TranscriptionError::ProviderUnavailable,
        _ => TranscriptionError::Protocol,
    }
}

/// Extract the transcript text from a Gemini response. A well-formed response
/// with no candidates yields an empty transcript (matching the legacy client);
/// unparseable JSON is a protocol error and never echoes the body.
fn parse_response(body: &str) -> Result<String, TranscriptionError> {
    let parsed: GenerateContentResponse =
        serde_json::from_str(body).map_err(|_| TranscriptionError::Protocol)?;
    let text = parsed
        .candidates
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.content)
        .and_then(|c| c.parts)
        .and_then(|p| p.into_iter().next())
        .and_then(|p| p.text)
        .unwrap_or_default();
    Ok(text.trim().to_string())
}

#[async_trait]
impl TranscriptionProvider for GeminiProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        let mut descriptor = descriptor();
        descriptor.models = vec![TranscriptionModelDescriptor {
            id: self.model.clone(),
            label: self.model.clone(),
        }];
        descriptor
    }

    async fn transcribe(
        &self,
        model_id: &str,
        request: &TranscriptionRequest,
        api_key: &str,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        if model_id != self.model {
            return Err(TranscriptionError::InvalidConfiguration(
                "Gemini model does not match the registered target".into(),
            ));
        }
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(TranscriptionError::MissingCredential);
        }

        let started = Instant::now();
        let wav = encode_wav(&request.audio_16khz_mono)?;
        let audio_base64 = base64::engine::general_purpose::STANDARD.encode(&wav);
        let payload = build_request(audio_base64);

        let url = format!("{}/models/{model_id}:generateContent", self.base_url);
        let response = self
            .http
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                let category = if err.is_timeout() {
                    TranscriptionError::Timeout
                } else {
                    TranscriptionError::Network
                };
                warn!("gemini transport error: {}", category.category());
                category
            })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // Never read the body: not logged/surfaced, and an unbounded
            // remote body must not force an unbounded allocation here.
            let err = map_status_error(status);
            warn!("gemini failed: http {} -> {}", status, err.category());
            return Err(err);
        }

        let body = response
            .text()
            .await
            .map_err(|_| TranscriptionError::Protocol)?;
        let text = parse_response(&body)?;

        let latency = TranscriptionLatency {
            total_ms: started.elapsed().as_millis() as u64,
        };
        debug!("gemini batch: ok in {} ms", latency.total_ms);

        Ok(TranscriptionResult {
            text,
            provider_id: PROVIDER_ID.to_string(),
            model_id: self.model.clone(),
            detected_language: None,
            latency,
        })
    }

    async fn test_connection(&self, api_key: &str) -> Result<(), TranscriptionError> {
        if api_key.trim().is_empty() {
            return Err(TranscriptionError::MissingCredential);
        }
        let response = self
            .http
            .get(format!("{}/models/{}", self.base_url, self.model))
            .header("x-goog-api-key", api_key.trim())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    TranscriptionError::Timeout
                } else {
                    TranscriptionError::ProviderUnavailable
                }
            })?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(map_status_error(status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn spawn_mock(response: String, delay: Option<Duration>) -> (String, Arc<Mutex<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_thread = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; 4096];
                loop {
                    match socket.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => captured_thread.lock().unwrap().extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                if let Some(d) = delay {
                    std::thread::sleep(d);
                }
                let _ = socket.write_all(response.as_bytes());
                let _ = socket.flush();
            }
        });
        (format!("http://{}", addr), captured)
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_line,
            body.as_bytes().len(),
            body
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn sample_request() -> TranscriptionRequest {
        TranscriptionRequest {
            audio_16khz_mono: vec![0.0, 0.05, -0.05, 0.1],
            language: None,
            custom_words: vec![],
        }
    }

    const SUCCESS_BODY: &str =
        r#"{"candidates":[{"content":{"parts":[{"text":"Bonjour le monde."}]}}]}"#;

    #[test]
    fn parses_transcript_text() {
        assert_eq!(parse_response(SUCCESS_BODY).unwrap(), "Bonjour le monde.");
    }

    #[test]
    fn empty_response_yields_empty_transcript() {
        assert_eq!(parse_response(r#"{"candidates":[]}"#).unwrap(), "");
        assert_eq!(parse_response(r#"{}"#).unwrap(), "");
    }

    #[test]
    fn invalid_json_is_protocol_error() {
        assert_eq!(
            parse_response("nonsense").unwrap_err(),
            TranscriptionError::Protocol
        );
    }

    #[test]
    fn status_mapping() {
        assert_eq!(map_status_error(400).category(), "invalid_configuration");
        assert_eq!(map_status_error(401).category(), "authentication");
        assert_eq!(map_status_error(429).category(), "rate_limited");
        assert_eq!(map_status_error(500).category(), "provider_unavailable");
    }

    #[test]
    fn descriptor_is_cloud_off_device() {
        let d = descriptor();
        assert_eq!(d.id, "gemini");
        assert_eq!(d.kind, ProviderKind::Cloud);
        assert!(d.capabilities.sends_audio_off_device);
        assert!(d.requires_credential);
    }

    #[test]
    fn configured_model_is_preserved_in_descriptor() {
        let provider = GeminiProvider::with_model("gemini-custom-preview");
        assert_eq!(provider.descriptor().models[0].id, "gemini-custom-preview");
    }

    #[test]
    fn empty_api_key_is_missing_credential() {
        let provider = GeminiProvider::with_base_url("http://127.0.0.1:1");
        let err = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), ""))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::MissingCredential);
    }

    #[test]
    fn whitespace_only_api_key_is_missing_credential() {
        let provider = GeminiProvider::with_base_url("http://127.0.0.1:1");
        let err = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "   \t  "))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::MissingCredential);
    }

    #[test]
    fn success_returns_text_and_sends_api_key_header() {
        let (base, captured) = spawn_mock(http_response("200 OK", SUCCESS_BODY), None);
        let provider = GeminiProvider::with_base_url(base);
        let result = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "gem-key"))
            .unwrap();
        assert_eq!(result.text, "Bonjour le monde.");
        assert_eq!(result.provider_id, "gemini");
        let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(request.to_lowercase().contains("x-goog-api-key"));
        assert!(request.contains("gem-key"));
    }

    #[test]
    fn api_key_with_surrounding_whitespace_is_sent_trimmed() {
        let (base, captured) = spawn_mock(http_response("200 OK", SUCCESS_BODY), None);
        let provider = GeminiProvider::with_base_url(base);
        runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "  gem-key  "))
            .unwrap();
        let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(request.contains("gem-key"));
        // The header line must carry exactly the trimmed value, not padding.
        let header_line = request
            .lines()
            .find(|line| line.to_lowercase().starts_with("x-goog-api-key"))
            .expect("api key header missing");
        assert_eq!(header_line.trim(), "x-goog-api-key: gem-key");
    }

    #[test]
    fn unauthorized_maps_to_authentication() {
        let (base, _) = spawn_mock(
            http_response("401 Unauthorized", r#"{"error":"nope"}"#),
            None,
        );
        let provider = GeminiProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "bad"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::Authentication);
    }

    #[test]
    fn rate_limited_maps_to_rate_limited() {
        let (base, _) = spawn_mock(http_response("429 Too Many Requests", "{}"), None);
        let provider = GeminiProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "k"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::RateLimited);
    }

    #[test]
    fn server_error_maps_to_provider_unavailable() {
        let (base, _) = spawn_mock(http_response("500 Internal Server Error", "{}"), None);
        let provider = GeminiProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "k"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::ProviderUnavailable);
    }

    #[test]
    fn slow_server_times_out() {
        let (base, _) = spawn_mock(
            http_response("200 OK", SUCCESS_BODY),
            Some(Duration::from_millis(600)),
        );
        let provider = GeminiProvider::with_base_url_and_timeout(base, Duration::from_millis(120));
        let err = runtime()
            .block_on(provider.transcribe(DEFAULT_MODEL, &sample_request(), "k"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::Timeout);
    }

    /// Send headers for a non-2xx status with a large declared `Content-Length`
    /// but never send the body, holding the connection open for `hold_open`.
    /// A bounded `tokio::time::timeout` around the call proves the client
    /// never attempts to read the body.
    fn spawn_mock_undelivered_body(status_line: &str, hold_open: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; 4096];
                loop {
                    match socket.read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let header = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: 10485760\r\n\r\n",
                    status_line
                );
                let _ = socket.write_all(header.as_bytes());
                let _ = socket.flush();
                std::thread::sleep(hold_open);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn error_with_undelivered_body_does_not_block_transcribe() {
        let base =
            spawn_mock_undelivered_body("500 Internal Server Error", Duration::from_millis(500));
        let provider = GeminiProvider::with_base_url(base);
        let outcome = runtime().block_on(async {
            tokio::time::timeout(
                Duration::from_millis(200),
                provider.transcribe(DEFAULT_MODEL, &sample_request(), "k"),
            )
            .await
        });
        let err = outcome
            .expect("must not block waiting on an undelivered error body")
            .unwrap_err();
        assert_eq!(err, TranscriptionError::ProviderUnavailable);
    }

    #[test]
    fn error_with_undelivered_body_does_not_block_test_connection() {
        let base =
            spawn_mock_undelivered_body("500 Internal Server Error", Duration::from_millis(500));
        let provider = GeminiProvider::with_base_url(base);
        let outcome = runtime().block_on(async {
            tokio::time::timeout(Duration::from_millis(200), provider.test_connection("k")).await
        });
        let err = outcome
            .expect("must not block waiting on an undelivered error body")
            .unwrap_err();
        assert_eq!(err, TranscriptionError::ProviderUnavailable);
    }
}
