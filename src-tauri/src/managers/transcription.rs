//! Transcription target orchestrator.
//!
//! Local engine ownership and lifecycle live in the private `local` module; cloud
//! implementations live behind the common provider contract. This manager only
//! selects the explicit target and preserves the existing synchronous API used by
//! the recording pipeline.

use std::sync::Arc;

use anyhow::Result;
use tauri::AppHandle;

use crate::managers::model::ModelManager;
use crate::settings::get_settings;
use crate::transcription::{TranscriptionProvider, TranscriptionTargetId};

#[path = "../transcription/local.rs"]
mod local;
use local::LocalProvider;
pub use local::ModelStateEvent;
pub use local::{RestoreOutcome, RestoreTicket};

use crate::transcription::TranscriptionError;

#[derive(Clone)]
pub struct TranscriptionManager {
    app_handle: AppHandle,
    local: Arc<LocalProvider>,
}

impl TranscriptionManager {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Result<Self> {
        Ok(Self {
            app_handle: app_handle.clone(),
            local: Arc::new(LocalProvider::new(app_handle, model_manager)?),
        })
    }

    fn selected_target(&self) -> Option<TranscriptionTargetId> {
        get_settings(&self.app_handle).selected_transcription_target
    }

    pub fn is_model_loaded(&self) -> bool {
        self.local.is_model_loaded()
    }

    pub fn unload_model(&self) -> Result<()> {
        self.local.unload_model()
    }

    pub fn maybe_unload_immediately(&self, context: &str) {
        self.local.maybe_unload_immediately(context)
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        self.local.load_model(model_id)
    }

    /// Switches the local model to `model_id` (if it isn't already loaded)
    /// and opens a [`RestoreTicket`] atomically with that switch, capturing
    /// whatever was loaded *before* it as the restore baseline. Used by
    /// temporary operations (long-audio fallback) that need the model
    /// switched for one call and reliably restored afterward via
    /// [`Self::restore_ticket`] -- see that method's docs for the exact
    /// unwind semantics when multiple such operations overlap.
    pub fn begin_temporary_local_switch(&self, model_id: &str) -> Result<RestoreTicket, String> {
        self.local
            .begin_temporary_switch(model_id)
            .map_err(|error| error.category().to_string())
    }

    /// Run a one-off local transcription against `model_id` (switching the
    /// loaded model first if needed) and return the text plus a
    /// [`RestoreTicket`] opened atomically with this call's own switch.
    /// The ticket is always returned, even when transcription itself
    /// failed after a successful switch, so the caller can always attempt
    /// to restore the previous model via [`Self::restore_ticket`] -- see
    /// `TicketGuard` below for making that unconditional regardless of
    /// which exit path the caller takes. Bypasses
    /// [`crate::transcription::TranscriptionService`] because this path is
    /// exclusively used by callers that already know the target is local
    /// (reprocess): the local provider ignores consent/credential concerns
    /// entirely.
    pub fn transcribe_local_ticketed(
        &self,
        model_id: &str,
        audio: Vec<f32>,
    ) -> (Result<String, TranscriptionError>, RestoreTicket) {
        self.local.transcribe_target_ticketed(model_id, audio)
    }

    /// Restore a [`RestoreTicket`] previously opened by
    /// [`Self::begin_temporary_local_switch`] or
    /// [`Self::transcribe_local_ticketed`]. Blocks until it is safe to do so
    /// (see [`RestoreTicket`]'s docs), unless a genuinely newer explicit
    /// selection landed after the ticket was opened, in which case it is
    /// correctly abandoned (`Superseded`) rather than clobbering that newer
    /// choice.
    pub fn restore_ticket(&self, ticket: RestoreTicket) -> RestoreOutcome {
        self.local.restore_ticket(ticket)
    }

    pub fn initiate_model_load(&self) {
        if self
            .selected_target()
            .as_ref()
            .is_some_and(|target| target.provider_id == "local")
        {
            self.local.initiate_model_load();
        }
    }

    pub fn get_current_model(&self) -> Option<String> {
        self.local.get_current_model()
    }

    pub fn get_current_model_name(&self) -> Option<String> {
        self.local.get_current_model_name()
    }

    pub fn get_model_name(&self, model_id: &str) -> Option<String> {
        self.local.get_model_name(model_id)
    }

    pub(crate) fn provider(&self) -> Arc<dyn TranscriptionProvider> {
        self.local.clone()
    }
}

/// RAII guard ensuring a [`RestoreTicket`] is always resolved, even if the
/// caller returns early (via `?` or otherwise) between opening it and the
/// point that would normally trigger the restore. Callers should still call
/// [`Self::resolve`] explicitly at every intended exit so a genuine restore
/// failure can be propagated to the caller fail-closed; `Drop` is only a
/// last-resort safety net (it cannot return a `Result`, so it can only log).
pub struct TicketGuard {
    manager: TranscriptionManager,
    ticket: Option<RestoreTicket>,
}

impl TicketGuard {
    pub fn new(manager: TranscriptionManager, ticket: RestoreTicket) -> Self {
        Self {
            manager,
            ticket: Some(ticket),
        }
    }

    /// Resolve the ticket now, returning the outcome. Idempotent: calling
    /// this again (or letting `Drop` run afterward) is a harmless no-op
    /// reported as `Restored`.
    pub fn resolve(&mut self) -> RestoreOutcome {
        match self.ticket.take() {
            Some(ticket) => self.manager.restore_ticket(ticket),
            None => RestoreOutcome::Restored,
        }
    }
}

impl Drop for TicketGuard {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            if let RestoreOutcome::Failed(error) = self.manager.restore_ticket(ticket) {
                log::error!(
                    "Failed to restore the previous local model state while unwinding: {}",
                    error
                );
            }
        }
    }
}
