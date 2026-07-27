//! ElevenLabs Scribe v2 batch (pre-recorded) transcription client.
//!
//! Contract is frozen in `docs/architecture/adr-001-cloud-transcription-providers.md`.
//! `POST {base}/v1/speech-to-text` with `xi-api-key` auth and a multipart form
//! carrying `model_id=scribe_v2` and the recorded audio as a 16 kHz mono WAV.
//!
//! The transport is deliberately thin: request building, response parsing and
//! HTTP-status → error mapping are pure functions covered by unit tests with
//! local fixtures, so the bulk of the client is verified without any network.
//! The client never logs the audio, the transcript, the API key, or the raw
//! provider response body — only sample counts, the HTTP status and the
//! normalized error category.

use std::io::Cursor;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hound::{SampleFormat, WavSpec, WavWriter};
use log::{debug, warn};
use serde::Deserialize;

use super::provider::TranscriptionProvider;
use super::types::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionError,
    TranscriptionLatency, TranscriptionModelDescriptor, TranscriptionRequest, TranscriptionResult,
};

/// Registry id for this provider.
pub const PROVIDER_ID: &str = "elevenlabs";
/// The batch model id sent as `model_id`.
pub const MODEL_SCRIBE_V2: &str = "scribe_v2";

const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";
const ENDPOINT_PATH: &str = "/v1/speech-to-text";
const CONNECTION_TEST_PATH: &str = "/v1/user";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// ElevenLabs Scribe v2 batch provider.
pub struct ElevenLabsProvider {
    base_url: String,
    http: reqwest::Client,
}

impl ElevenLabsProvider {
    /// Production constructor targeting the official API base URL.
    pub fn new() -> Self {
        Self::build(DEFAULT_BASE_URL.to_string(), DEFAULT_TIMEOUT)
    }

    fn build(base_url: String, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self { base_url, http }
    }

    /// Test-only constructor allowing a mock server base URL to be injected. The
    /// base URL is never sourced from user settings in production.
    #[cfg(test)]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::build(base_url.into(), DEFAULT_TIMEOUT)
    }

    /// Test-only constructor with a custom timeout for exercising timeouts.
    #[cfg(test)]
    pub fn with_base_url_and_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self::build(base_url.into(), timeout)
    }
}

impl Default for ElevenLabsProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Serializable descriptor. Free-standing so the catalog can be built without a
/// live client instance.
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        id: PROVIDER_ID.to_string(),
        label: "ElevenLabs Scribe".to_string(),
        kind: ProviderKind::Cloud,
        models: vec![TranscriptionModelDescriptor {
            id: MODEL_SCRIBE_V2.to_string(),
            label: "Scribe v2 (batch)".to_string(),
        }],
        capabilities: ProviderCapabilities {
            batch: true,
            realtime: false,
            // Empty = any/auto-detected. French is in the top accuracy tier.
            supported_languages: vec![],
            supports_word_timestamps: true,
            sends_audio_off_device: true,
        },
        requires_credential: true,
        privacy_url: Some("https://elevenlabs.io/privacy-policy".to_string()),
        pricing_url: Some("https://elevenlabs.io/pricing/api".to_string()),
        cost_text: Some("≈ $0.22 / hour of audio".to_string()),
        retention_text: Some(
            "Retention follows your ElevenLabs account and provider policy settings.".to_string(),
        ),
        consent_version: 1,
        beta: true,
    }
}

/// Encode 16 kHz mono `f32` samples to a WAV byte buffer (PCM16), mirroring the
/// Gemini client's encoder.
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
            let sample_i16 = (clamped * i16::MAX as f32) as i16;
            writer
                .write_sample(sample_i16)
                .map_err(|_| TranscriptionError::InvalidConfiguration("wav sample".to_string()))?;
        }
        writer
            .finalize()
            .map_err(|_| TranscriptionError::InvalidConfiguration("wav finalize".to_string()))?;
    }
    Ok(buffer)
}

fn valid_keyterms(custom_words: &[String]) -> impl Iterator<Item = String> + '_ {
    custom_words
        .iter()
        .map(|term| term.trim())
        .filter(|term| {
            !term.is_empty()
                && term.chars().count() < 50
                && term.split_whitespace().count() <= 5
                && !term.chars().any(|character| "<>{}[]\\".contains(character))
        })
        .take(1000)
        .map(str::to_owned)
}

/// Map a non-2xx HTTP status to a normalized error category (ADR §2.4). The
/// response body is intentionally *not* consulted so it can never leak.
fn map_status_error(status: u16) -> TranscriptionError {
    match status {
        400 | 422 => TranscriptionError::InvalidConfiguration(format!(
            "provider rejected request ({status})"
        )),
        401 | 403 => TranscriptionError::Authentication,
        402 => TranscriptionError::Quota,
        429 => TranscriptionError::RateLimited,
        500..=599 => TranscriptionError::ProviderUnavailable,
        _ => TranscriptionError::Protocol,
    }
}

/// Classify a transport-level reqwest failure.
fn classify_send_error(err: &reqwest::Error) -> TranscriptionError {
    if err.is_timeout() {
        TranscriptionError::Timeout
    } else {
        TranscriptionError::Network
    }
}

/// Minimal view of the single-channel success response (ADR §2.3). Only the
/// fields Parler's batch path reads are declared; everything else is ignored.
#[derive(Deserialize)]
struct SpeechToTextResponse {
    text: String,
    #[serde(default)]
    language_code: Option<String>,
}

struct ParsedTranscript {
    text: String,
    detected_language: Option<String>,
}

/// Parse a success body into the fields Parler needs. Unparseable JSON maps to
/// `Protocol` without echoing the body.
fn parse_transcript(body: &str) -> Result<ParsedTranscript, TranscriptionError> {
    let parsed: SpeechToTextResponse =
        serde_json::from_str(body).map_err(|_| TranscriptionError::Protocol)?;
    Ok(ParsedTranscript {
        text: parsed.text.trim().to_string(),
        detected_language: parsed.language_code,
    })
}

#[async_trait]
impl TranscriptionProvider for ElevenLabsProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        descriptor()
    }

    async fn transcribe(
        &self,
        model_id: &str,
        request: &TranscriptionRequest,
        api_key: &str,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        if model_id != MODEL_SCRIBE_V2 {
            return Err(TranscriptionError::InvalidConfiguration(
                "unsupported ElevenLabs model".into(),
            ));
        }
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(TranscriptionError::MissingCredential);
        }

        let started = Instant::now();
        debug!(
            "elevenlabs batch: encoding {} samples",
            request.audio_16khz_mono.len()
        );

        let wav = encode_wav(&request.audio_16khz_mono)?;

        let file_part = reqwest::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|_| TranscriptionError::Protocol)?;

        let mut form = reqwest::multipart::Form::new()
            .text("model_id", model_id.to_string())
            .part("file", file_part);

        // Send an explicit language only when the user chose a concrete one;
        // "auto" / None lets the provider auto-detect.
        if let Some(language) = request.language.as_ref() {
            if !language.is_empty() && language != "auto" {
                form = form.text("language_code", language.clone());
            }
        }
        // OpenAPI multipart arrays use form/explode semantics: repeat the
        // `keyterms` field once per valid custom term.
        for keyterm in valid_keyterms(&request.custom_words) {
            form = form.text("keyterms", keyterm);
        }

        let url = format!("{}{}", self.base_url, ENDPOINT_PATH);
        let response = self
            .http
            .post(&url)
            .header("xi-api-key", api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|err| {
                let category = classify_send_error(&err);
                warn!("elevenlabs batch transport error: {}", category.category());
                category
            })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // The body is never read: it's never logged or surfaced, and
            // reading it would let a remote server force an unbounded
            // allocation. Dropping `response` here closes the connection
            // instead of reusing it, which is an acceptable trade-off.
            let err = map_status_error(status);
            warn!(
                "elevenlabs batch failed: http {} -> {}",
                status,
                err.category()
            );
            return Err(err);
        }

        let body = response
            .text()
            .await
            .map_err(|_| TranscriptionError::Protocol)?;
        let parsed = parse_transcript(&body)?;

        let latency = TranscriptionLatency {
            total_ms: started.elapsed().as_millis() as u64,
        };
        debug!("elevenlabs batch: ok in {} ms", latency.total_ms);

        Ok(TranscriptionResult {
            text: parsed.text,
            provider_id: PROVIDER_ID.to_string(),
            model_id: MODEL_SCRIBE_V2.to_string(),
            detected_language: parsed.detected_language,
            latency,
        })
    }

    async fn test_connection(&self, api_key: &str) -> Result<(), TranscriptionError> {
        if api_key.trim().is_empty() {
            return Err(TranscriptionError::MissingCredential);
        }
        let response = self
            .http
            .get(format!("{}{}", self.base_url, CONNECTION_TEST_PATH))
            .header("xi-api-key", api_key.trim())
            .send()
            .await
            .map_err(|error| classify_send_error(&error))?;
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

    const SUCCESS_FR: &str = include_str!("fixtures/elevenlabs/success_fr.json");

    // ---- Pure-function tests (no network) -------------------------------

    #[test]
    fn encodes_wav_with_riff_header() {
        let wav = encode_wav(&[0.0, 0.1, -0.1, 0.5]).unwrap();
        assert!(wav.len() > 44, "wav should have header + data");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    #[test]
    fn status_mapping_matches_adr() {
        assert_eq!(map_status_error(400).category(), "invalid_configuration");
        assert_eq!(map_status_error(422).category(), "invalid_configuration");
        assert_eq!(map_status_error(401).category(), "authentication");
        assert_eq!(map_status_error(403).category(), "authentication");
        assert_eq!(map_status_error(402).category(), "quota");
        assert_eq!(map_status_error(429).category(), "rate_limited");
        assert_eq!(map_status_error(500).category(), "provider_unavailable");
        assert_eq!(map_status_error(503).category(), "provider_unavailable");
        assert_eq!(map_status_error(418).category(), "protocol");
    }

    #[test]
    fn parses_success_fixture() {
        let parsed = parse_transcript(SUCCESS_FR).unwrap();
        assert_eq!(parsed.text, "Bonjour, ceci est une dictée de test.");
        assert_eq!(parsed.detected_language.as_deref(), Some("fr"));
    }

    #[test]
    fn invalid_json_is_protocol_error() {
        let error = parse_transcript("not json")
            .err()
            .expect("invalid JSON must fail");
        assert_eq!(error, TranscriptionError::Protocol);
    }

    #[test]
    fn empty_transcript_is_not_treated_as_a_protocol_error() {
        let parsed = parse_transcript(r#"{"text":"   ","language_code":"fr"}"#).unwrap();
        assert!(parsed.text.is_empty());
    }

    #[test]
    fn missing_text_field_is_protocol_error() {
        let error = parse_transcript(r#"{"language_code":"fr"}"#)
            .err()
            .expect("a response without text must fail");
        assert_eq!(error, TranscriptionError::Protocol);
    }

    #[test]
    fn descriptor_declares_cloud_and_off_device() {
        let d = descriptor();
        assert_eq!(d.id, "elevenlabs");
        assert_eq!(d.kind, ProviderKind::Cloud);
        assert!(d.capabilities.sends_audio_off_device);
        assert!(d.requires_credential);
        assert!(d.capabilities.batch);
        assert!(!d.capabilities.realtime);
        assert!(d.models.iter().any(|m| m.id == "scribe_v2"));
        assert!(d.privacy_url.is_some());
    }

    // ---- HTTP-level tests against a minimal mock server -----------------

    /// Spawn a one-shot HTTP/1.1 server on a std thread. It reads the request
    /// (capturing it for assertions), then writes `response`. Returns the base
    /// URL. Using std TcpListener keeps the test runtime-agnostic.
    fn spawn_mock(response: String, delay: Option<Duration>) -> (String, Arc<Mutex<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_thread = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                // Read until a short idle gap so the full request (headers +
                // multipart body) is captured before we respond.
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; 4096];
                loop {
                    match socket.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => captured_thread.lock().unwrap().extend_from_slice(&buf[..n]),
                        Err(_) => break, // timeout/would-block: request fully read
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
            audio_16khz_mono: vec![0.0, 0.05, -0.05, 0.1, 0.0],
            language: Some("fr".to_string()),
            custom_words: vec![],
        }
    }

    #[test]
    fn empty_api_key_is_missing_credential_without_network() {
        let provider = ElevenLabsProvider::with_base_url("http://127.0.0.1:1");
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), ""))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::MissingCredential);
    }

    #[test]
    fn whitespace_only_api_key_is_missing_credential_without_network() {
        let provider = ElevenLabsProvider::with_base_url("http://127.0.0.1:1");
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "   \t  "))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::MissingCredential);
    }

    #[test]
    fn connection_test_uses_user_endpoint_without_audio() {
        let (base, captured) = spawn_mock(http_response("200 OK", "{}"), None);
        let provider = ElevenLabsProvider::with_base_url(base);
        runtime()
            .block_on(provider.test_connection("test-key"))
            .unwrap();

        let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(request.starts_with("GET /v1/user "));
        assert!(request.contains("test-key"));
        assert!(!request.contains("audio.wav"));
        assert!(!request.contains("RIFF"));
    }

    #[test]
    fn success_returns_text_and_sends_auth_header() {
        let (base, captured) = spawn_mock(http_response("200 OK", SUCCESS_FR), None);
        let provider = ElevenLabsProvider::with_base_url(base);
        let mut request = sample_request();
        request.custom_words = vec!["ParlerUniqueTerm".to_string()];

        let result = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &request, "test-key"))
            .unwrap();

        assert_eq!(result.text, "Bonjour, ceci est une dictée de test.");
        assert_eq!(result.provider_id, "elevenlabs");
        assert_eq!(result.model_id, "scribe_v2");
        assert_eq!(result.detected_language.as_deref(), Some("fr"));

        let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(
            request.to_lowercase().contains("xi-api-key"),
            "auth header name missing from request"
        );
        assert!(request.contains("test-key"), "auth header value missing");
        assert!(request.contains("model_id"), "model_id field missing");
        assert!(request.contains("scribe_v2"), "model value missing");
        assert!(
            request.contains("language_code") && request.contains("fr"),
            "explicit language missing"
        );
        assert!(
            request
                .contains("Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"")
                || request.contains("filename=\"audio.wav\""),
            "multipart file metadata missing"
        );
        assert!(
            request.contains("Content-Type: audio/wav"),
            "WAV MIME missing"
        );
        assert!(
            request.contains("keyterms") && request.contains("ParlerUniqueTerm"),
            "custom words must be sent as repeated keyterms fields"
        );
        let bytes = captured.lock().unwrap();
        let wav_start = bytes
            .windows(4)
            .position(|window| window == b"RIFF")
            .expect("multipart payload is not WAV");
        let reader = hound::WavReader::new(Cursor::new(&bytes[wav_start..])).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        // The key must never appear anywhere but the auth header value; it does
        // here because we sent it, which is expected. Confirm it is not echoed
        // into any result field.
        assert!(!result.text.contains("test-key"));
    }

    #[test]
    fn api_key_with_surrounding_whitespace_is_sent_trimmed() {
        let (base, captured) = spawn_mock(http_response("200 OK", SUCCESS_FR), None);
        let provider = ElevenLabsProvider::with_base_url(base);
        runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "  test-key  "))
            .unwrap();
        let request = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        let header_line = request
            .lines()
            .find(|line| line.to_lowercase().starts_with("xi-api-key"))
            .expect("api key header missing");
        assert_eq!(header_line.trim(), "xi-api-key: test-key");
    }

    #[test]
    fn invalid_json_from_http_is_protocol_error() {
        let (base, _) = spawn_mock(http_response("200 OK", "not-json"), None);
        let provider = ElevenLabsProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "test-key"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::Protocol);
    }

    #[test]
    fn unsupported_keyterms_are_filtered_before_multipart_encoding() {
        let terms = vec![
            " valid term ".to_string(),
            "contains [ bracket".to_string(),
            "one two three four five six".to_string(),
            "x".repeat(50),
            String::new(),
        ];
        assert_eq!(
            valid_keyterms(&terms).collect::<Vec<_>>(),
            vec!["valid term"]
        );
    }

    #[test]
    fn unauthorized_maps_to_authentication() {
        let (base, _) = spawn_mock(
            http_response(
                "401 Unauthorized",
                r#"{"detail":{"code":"invalid_api_key"}}"#,
            ),
            None,
        );
        let provider = ElevenLabsProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "bad-key"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::Authentication);
    }

    #[test]
    fn rate_limited_maps_to_rate_limited() {
        let (base, _) = spawn_mock(
            http_response(
                "429 Too Many Requests",
                r#"{"detail":{"code":"system_busy"}}"#,
            ),
            None,
        );
        let provider = ElevenLabsProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "k"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::RateLimited);
    }

    #[test]
    fn server_error_maps_to_provider_unavailable() {
        let (base, _) = spawn_mock(http_response("503 Service Unavailable", "{}"), None);
        let provider = ElevenLabsProvider::with_base_url(base);
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "k"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::ProviderUnavailable);
    }

    #[test]
    fn slow_server_times_out() {
        let (base, _) = spawn_mock(
            http_response("200 OK", SUCCESS_FR),
            Some(Duration::from_millis(600)),
        );
        let provider =
            ElevenLabsProvider::with_base_url_and_timeout(base, Duration::from_millis(120));
        let err = runtime()
            .block_on(provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "k"))
            .unwrap_err();
        assert_eq!(err, TranscriptionError::Timeout);
    }

    /// Send headers for a non-2xx status with a large declared `Content-Length`
    /// but never actually send the body, holding the connection open for
    /// `hold_open`. If the client ever tried to read the body it would block
    /// for the full `hold_open` duration; a bounded `tokio::time::timeout`
    /// around the call proves it doesn't.
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
        let provider = ElevenLabsProvider::with_base_url(base);
        let outcome = runtime().block_on(async {
            tokio::time::timeout(
                Duration::from_millis(200),
                provider.transcribe(MODEL_SCRIBE_V2, &sample_request(), "k"),
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
        let provider = ElevenLabsProvider::with_base_url(base);
        let outcome = runtime().block_on(async {
            tokio::time::timeout(Duration::from_millis(200), provider.test_connection("k")).await
        });
        let err = outcome
            .expect("must not block waiting on an undelivered error body")
            .unwrap_err();
        assert_eq!(err, TranscriptionError::ProviderUnavailable);
    }
}
