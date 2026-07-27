//! The provider contract every transcription backend implements.
//!
//! Milestone 1 covers the **batch** path only. Realtime streaming
//! (`StreamingSession`) is intentionally deferred to a later PR so this trait
//! stays small and every implementation is fully testable now. Adding Azure or
//! AssemblyAI later means adding a new implementor and a registry entry — the
//! central recording/correction/paste pipeline does not branch per provider.

use async_trait::async_trait;

use super::types::{
    ProviderDescriptor, TranscriptionError, TranscriptionRequest, TranscriptionResult,
};

/// A transcription backend (cloud or local wrapper).
///
/// Implementors must never log the audio, the transcript, or the API key. On
/// failure they return a normalized [`TranscriptionError`]; the raw provider
/// response body is not propagated.
#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    /// Static, serializable description of the provider for the UI catalog.
    fn descriptor(&self) -> ProviderDescriptor;

    /// Transcribe a complete recording. `api_key` is fetched by the caller from
    /// the OS keyring and passed in; the provider never reads it from settings.
    async fn transcribe(
        &self,
        model_id: &str,
        request: &TranscriptionRequest,
        api_key: &str,
    ) -> Result<TranscriptionResult, TranscriptionError>;

    /// Validate a configured credential without uploading audio.
    async fn test_connection(&self, _api_key: &str) -> Result<(), TranscriptionError> {
        Err(TranscriptionError::InvalidConfiguration(
            "connection test is not supported for this provider".into(),
        ))
    }
}
