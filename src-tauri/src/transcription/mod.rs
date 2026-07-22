//! Transcription provider abstraction.
//!
//! Separates *transcription targets* (what the user selects to turn speech into
//! text) from the management of downloadable local model files. A
//! [`TranscriptionRegistry`] maps a [`TranscriptionTargetId`] to a concrete
//! [`TranscriptionProvider`] that returns a normalized [`TranscriptionResult`].
//!
//! The module is free of any `transcribe-rs` dependency so it compiles and is
//! unit-tested under the CI mock build. Local engines keep running through their
//! existing, proven path in `managers::transcription`; this module owns cloud
//! providers and the serializable catalog surfaced to the UI.

pub mod provider;
pub mod registry;
pub mod types;

pub use provider::TranscriptionProvider;
pub use registry::TranscriptionRegistry;
pub use types::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionError,
    TranscriptionLatency, TranscriptionModelDescriptor, TranscriptionRequest, TranscriptionResult,
    TranscriptionTargetId,
};
