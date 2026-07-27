// CI-only mock TranscriptionManager - avoids whisper/Vulkan dependencies.
// This file is copied over transcription.rs during CI tests.
// Existing tests don't exercise transcription, so this is safe.

use crate::managers::model::ModelManager;
use crate::transcription::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionError,
    TranscriptionLatency, TranscriptionModelDescriptor, TranscriptionProvider,
    TranscriptionRequest, TranscriptionResult,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;
use tauri::AppHandle;

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

/// Mirrors `transcription::local::RestoreOutcome`: the mock never actually
/// supersedes or fails a restore, but the type must exist so callers
/// (`commands/history.rs`, `actions.rs`) compile identically against both
/// the real and mock managers.
#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored,
    Superseded,
    Failed(String),
}

/// Mirrors `transcription::local::RestoreTicket`. The mock never switches
/// any real model, so there is nothing to track; it exists purely so the
/// ticket-based restore API has the same shape under CI's mock swap.
pub struct RestoreTicket;

#[derive(Clone)]
pub struct TranscriptionManager {
    #[allow(dead_code)]
    app_handle: AppHandle,
}

struct MockLocalProvider;

#[async_trait]
impl TranscriptionProvider for MockLocalProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: "local".into(),
            label: "Local".into(),
            kind: ProviderKind::Local,
            models: vec![TranscriptionModelDescriptor {
                id: "mock-local".into(),
                label: "Mock local".into(),
            }],
            capabilities: ProviderCapabilities {
                batch: true,
                realtime: false,
                supported_languages: vec![],
                supports_word_timestamps: false,
                sends_audio_off_device: false,
            },
            requires_credential: false,
            privacy_url: None,
            pricing_url: None,
            cost_text: None,
            retention_text: None,
            consent_version: 0,
            beta: false,
        }
    }

    async fn transcribe(
        &self,
        model_id: &str,
        _request: &TranscriptionRequest,
        _api_key: &str,
    ) -> std::result::Result<TranscriptionResult, TranscriptionError> {
        Ok(TranscriptionResult {
            text: String::new(),
            provider_id: "local".into(),
            model_id: model_id.into(),
            detected_language: None,
            latency: TranscriptionLatency::default(),
        })
    }
}

impl TranscriptionManager {
    pub fn new(app_handle: &AppHandle, _model_manager: Arc<ModelManager>) -> Result<Self> {
        Ok(Self {
            app_handle: app_handle.clone(),
        })
    }

    pub fn is_model_loaded(&self) -> bool {
        false
    }

    pub fn unload_model(&self) -> Result<()> {
        Ok(())
    }

    pub fn maybe_unload_immediately(&self, _context: &str) {}

    pub fn load_model(&self, _model_id: &str) -> Result<()> {
        Ok(())
    }

    pub fn initiate_model_load(&self) {}

    pub fn get_current_model(&self) -> Option<String> {
        None
    }

    pub fn get_current_model_name(&self) -> Option<String> {
        None
    }

    pub fn get_model_name(&self, _model_id: &str) -> Option<String> {
        None
    }

    pub(crate) fn provider(&self) -> Arc<dyn TranscriptionProvider> {
        Arc::new(MockLocalProvider)
    }

    pub fn transcribe(&self, _audio: Vec<f32>) -> Result<String> {
        Ok(String::new())
    }

    pub fn begin_temporary_local_switch(&self, _model_id: &str) -> Result<RestoreTicket, String> {
        Ok(RestoreTicket)
    }

    pub fn transcribe_local_ticketed(
        &self,
        model_id: &str,
        _audio: Vec<f32>,
    ) -> (
        std::result::Result<String, TranscriptionError>,
        RestoreTicket,
    ) {
        (
            Ok(TranscriptionResult {
                text: String::new(),
                provider_id: "local".into(),
                model_id: model_id.into(),
                detected_language: None,
                latency: TranscriptionLatency::default(),
            }
            .text),
            RestoreTicket,
        )
    }

    pub fn restore_ticket(&self, _ticket: RestoreTicket) -> RestoreOutcome {
        RestoreOutcome::Restored
    }
}

/// Mirrors `managers::transcription::TicketGuard`'s public surface used by
/// `commands/history.rs`/`actions.rs`.
pub struct TicketGuard {
    ticket: Option<RestoreTicket>,
}

impl TicketGuard {
    pub fn new(_manager: TranscriptionManager, ticket: RestoreTicket) -> Self {
        Self {
            ticket: Some(ticket),
        }
    }

    pub fn resolve(&mut self) -> RestoreOutcome {
        self.ticket.take();
        RestoreOutcome::Restored
    }
}
