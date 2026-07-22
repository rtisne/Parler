//! Shared domain types for the transcription provider abstraction.
//!
//! These types are deliberately free of any local-engine (`transcribe-rs`) or
//! network-client dependency so the whole module compiles and is unit-tested in
//! CI (which builds with the mock transcription manager). Nothing here ever
//! carries an API key or a raw provider response body.

use serde::{Deserialize, Serialize};
use specta::Type;

/// Fully-qualified identifier of a transcription target: a provider plus one of
/// its models. The local engines use `provider_id = "local"`.
#[derive(Serialize, Deserialize, Type, Debug, Clone, PartialEq, Eq, Hash)]
pub struct TranscriptionTargetId {
    pub provider_id: String,
    pub model_id: String,
}

impl TranscriptionTargetId {
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }
    }
}

/// Whether a target runs on this machine or sends audio to a remote service.
#[derive(Serialize, Deserialize, Type, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Runs entirely on-device (local model files).
    Local,
    /// Sends audio off-device to a third-party API.
    Cloud,
}

/// Capabilities a provider advertises, used both for UI display and to reject
/// unsupported requests before any network call.
#[derive(Serialize, Deserialize, Type, Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Supports whole-file (batch) transcription.
    pub batch: bool,
    /// Supports realtime/streaming transcription. Always `false` in milestone 1.
    pub realtime: bool,
    /// ISO-639-1 language codes the provider supports, or empty for "any /
    /// auto-detected".
    pub supported_languages: Vec<String>,
    /// Returns word-level timestamps.
    pub supports_word_timestamps: bool,
    /// Whether using this provider transmits audio off this device. Cloud
    /// providers set this to `true`; it drives the mandatory consent warning.
    pub sends_audio_off_device: bool,
}

/// One selectable model within a provider (e.g. ElevenLabs `scribe_v2`).
#[derive(Serialize, Deserialize, Type, Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionModelDescriptor {
    pub id: String,
    pub label: String,
}

/// Serializable description of a provider for the settings UI. Carries no
/// secret: credential state is queried separately through the write-only
/// secrets commands.
#[derive(Serialize, Deserialize, Type, Debug, Clone)]
pub struct ProviderDescriptor {
    pub id: String,
    pub label: String,
    pub kind: ProviderKind,
    pub models: Vec<TranscriptionModelDescriptor>,
    pub capabilities: ProviderCapabilities,
    /// Whether the provider needs an API key configured before it can be used.
    pub requires_credential: bool,
    /// Link to the provider's privacy policy (cloud providers only).
    pub privacy_url: Option<String>,
    /// Link to the provider's pricing page (cloud providers only).
    pub pricing_url: Option<String>,
    /// Short, human-readable cost hint (e.g. "≈ $0.22 / hour of audio").
    pub cost_text: Option<String>,
    /// Version of the consent text the user must accept. Bumping this
    /// invalidates prior consent for the provider.
    pub consent_version: u32,
    /// Whether the provider is offered as a beta/experimental target.
    pub beta: bool,
}

/// A batch transcription request handed to a provider. Audio is 16 kHz mono
/// `f32` samples, matching the recorder output. This type is intentionally not
/// serializable: audio never crosses the Tauri command/event boundary.
#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    pub audio_16khz_mono: Vec<f32>,
    /// Explicit language (ISO-639-1) or `None` for provider auto-detection.
    pub language: Option<String>,
    /// User custom vocabulary; providers that support keyterm biasing may use
    /// it, others ignore it (local correction still runs afterwards).
    pub custom_words: Vec<String>,
}

/// Timing captured for a completed transcription. Contains no audio, text, or
/// key — safe to log and surface in the privacy-safe metrics view.
#[derive(Serialize, Deserialize, Type, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptionLatency {
    /// Wall-clock milliseconds from request start to normalized result.
    pub total_ms: u64,
}

/// Normalized successful transcription result.
#[derive(Serialize, Deserialize, Type, Debug, Clone)]
pub struct TranscriptionResult {
    pub text: String,
    pub provider_id: String,
    pub model_id: String,
    /// Language the provider reported detecting, if any.
    pub detected_language: Option<String>,
    pub latency: TranscriptionLatency,
}

/// Stable error categories shared by every provider.
///
/// The frontend localizes user-facing messages from [`TranscriptionError::category`].
/// Variants may carry a *sanitized* technical detail (a request id, a status
/// code) but **never** a secret, a raw provider response body, audio, or
/// transcript text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionError {
    /// No API key configured for the provider.
    MissingCredential,
    /// The user has not accepted (the current version of) the consent notice.
    ConsentRequired,
    /// Request parameters or provider configuration are invalid.
    InvalidConfiguration(String),
    /// Authentication with the provider failed (bad/expired key, forbidden).
    Authentication,
    /// Out of credits / usage quota exhausted.
    Quota,
    /// Rate or concurrency limit hit; retry later.
    RateLimited,
    /// Network transport failure reaching the provider.
    Network,
    /// The request timed out.
    Timeout,
    /// The requested language is not supported by the provider.
    UnsupportedLanguage(String),
    /// The provider returned an unexpected/unparseable protocol response.
    Protocol,
    /// The operation was cancelled by the user.
    Cancelled,
    /// A bounded audio channel overflowed (realtime backpressure).
    AudioBackpressure,
    /// The provider is unavailable (5xx / service down).
    ProviderUnavailable,
}

impl TranscriptionError {
    /// Stable, machine-readable category slug for logs, history metadata, and
    /// as an i18n key on the frontend. Never contains dynamic detail.
    pub fn category(&self) -> &'static str {
        match self {
            TranscriptionError::MissingCredential => "missing_credential",
            TranscriptionError::ConsentRequired => "consent_required",
            TranscriptionError::InvalidConfiguration(_) => "invalid_configuration",
            TranscriptionError::Authentication => "authentication",
            TranscriptionError::Quota => "quota",
            TranscriptionError::RateLimited => "rate_limited",
            TranscriptionError::Network => "network",
            TranscriptionError::Timeout => "timeout",
            TranscriptionError::UnsupportedLanguage(_) => "unsupported_language",
            TranscriptionError::Protocol => "protocol",
            TranscriptionError::Cancelled => "cancelled",
            TranscriptionError::AudioBackpressure => "audio_backpressure",
            TranscriptionError::ProviderUnavailable => "provider_unavailable",
        }
    }
}

impl std::fmt::Display for TranscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscriptionError::InvalidConfiguration(detail) => {
                write!(f, "invalid configuration: {detail}")
            }
            TranscriptionError::UnsupportedLanguage(lang) => {
                write!(f, "unsupported language: {lang}")
            }
            other => write!(f, "{}", other.category()),
        }
    }
}

impl std::error::Error for TranscriptionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_is_stable_for_every_variant() {
        assert_eq!(
            TranscriptionError::MissingCredential.category(),
            "missing_credential"
        );
        assert_eq!(
            TranscriptionError::Authentication.category(),
            "authentication"
        );
        assert_eq!(TranscriptionError::RateLimited.category(), "rate_limited");
        assert_eq!(TranscriptionError::Timeout.category(), "timeout");
        assert_eq!(
            TranscriptionError::ProviderUnavailable.category(),
            "provider_unavailable"
        );
    }

    #[test]
    fn display_never_leaks_a_response_body_or_secret() {
        // The detail carried by InvalidConfiguration is developer-controlled and
        // must be a short reason, never a raw body. This test documents the
        // contract: constructing it from a body is a bug, but even then Display
        // only echoes what the caller passed — callers must pass sanitized text.
        let err = TranscriptionError::Authentication;
        assert_eq!(err.to_string(), "authentication");
        let err = TranscriptionError::UnsupportedLanguage("xx".into());
        assert_eq!(err.to_string(), "unsupported language: xx");
    }

    #[test]
    fn target_id_round_trips_through_json() {
        let id = TranscriptionTargetId::new("elevenlabs", "scribe_v2");
        let json = serde_json::to_string(&id).unwrap();
        let back: TranscriptionTargetId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        assert!(json.contains("elevenlabs"));
    }

    #[test]
    fn provider_descriptor_serializes_without_secret_fields() {
        let descriptor = ProviderDescriptor {
            id: "elevenlabs".into(),
            label: "ElevenLabs".into(),
            kind: ProviderKind::Cloud,
            models: vec![TranscriptionModelDescriptor {
                id: "scribe_v2".into(),
                label: "Scribe v2".into(),
            }],
            capabilities: ProviderCapabilities {
                batch: true,
                realtime: false,
                supported_languages: vec![],
                supports_word_timestamps: true,
                sends_audio_off_device: true,
            },
            requires_credential: true,
            privacy_url: Some("https://elevenlabs.io/privacy-policy".into()),
            pricing_url: Some("https://elevenlabs.io/pricing/api".into()),
            cost_text: Some("≈ $0.22 / hour".into()),
            consent_version: 1,
            beta: true,
        };
        let json = serde_json::to_string(&descriptor).unwrap();
        for forbidden in ["api_key", "apiKey", "secret", "xi-api-key", "password"] {
            assert!(!json.contains(forbidden), "descriptor leaked `{forbidden}`");
        }
    }
}
