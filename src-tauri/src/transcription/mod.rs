//! Transcription provider abstraction.
//!
//! Separates *transcription targets* (what the user selects to turn speech into
//! text) from the management of downloadable local model files. A
//! [`TranscriptionRegistry`] maps a [`TranscriptionTargetId`] to a concrete
//! [`TranscriptionProvider`] that returns a normalized [`TranscriptionResult`].
//!
//! Local engines are isolated behind the manager's `LocalProvider`; cloud
//! implementations are registered here.

pub mod elevenlabs;
pub mod gemini;

pub mod provider;
pub mod registry;
pub mod service;
pub mod types;

pub use elevenlabs::ElevenLabsProvider;
pub use gemini::GeminiProvider;

pub use provider::TranscriptionProvider;
pub use registry::TranscriptionRegistry;
pub use service::TranscriptionService;
pub use types::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionError,
    TranscriptionLatency, TranscriptionModelDescriptor, TranscriptionRequest, TranscriptionResult,
    TranscriptionTargetId,
};
