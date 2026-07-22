//! Registry that maps a [`TranscriptionTargetId`] to a concrete provider.
//!
//! Resolution is strict: an unknown provider or model is an error, never a
//! silent fallback to some default. This enforces roadmap invariant #4 — no
//! implicit switch between targets.

use std::collections::HashMap;
use std::sync::Arc;

use super::provider::TranscriptionProvider;
use super::types::{ProviderDescriptor, TranscriptionError, TranscriptionTargetId};

/// Holds every registered [`TranscriptionProvider`] keyed by provider id.
#[derive(Clone, Default)]
pub struct TranscriptionRegistry {
    providers: HashMap<String, Arc<dyn TranscriptionProvider>>,
}

impl TranscriptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a provider. The provider's descriptor id is the key.
    pub fn register(&mut self, provider: Arc<dyn TranscriptionProvider>) {
        let id = provider.descriptor().id;
        self.providers.insert(id, provider);
    }

    /// Look up a provider by id without validating a model.
    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn TranscriptionProvider>> {
        self.providers.get(provider_id).cloned()
    }

    /// Serializable descriptors for every registered provider, for the UI
    /// catalog. Order is not guaranteed; the frontend sorts for display.
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers.values().map(|p| p.descriptor()).collect()
    }

    /// Resolve a target to its provider, verifying the provider exists and
    /// advertises the requested model. Returns [`TranscriptionError::InvalidConfiguration`]
    /// rather than falling back to any default.
    pub fn resolve(
        &self,
        target: &TranscriptionTargetId,
    ) -> Result<Arc<dyn TranscriptionProvider>, TranscriptionError> {
        let provider = self.get(&target.provider_id).ok_or_else(|| {
            TranscriptionError::InvalidConfiguration(format!(
                "unknown provider `{}`",
                target.provider_id
            ))
        })?;

        let descriptor = provider.descriptor();
        let model_known = descriptor.models.iter().any(|m| m.id == target.model_id);
        if !model_known {
            return Err(TranscriptionError::InvalidConfiguration(format!(
                "provider `{}` has no model `{}`",
                target.provider_id, target.model_id
            )));
        }

        Ok(provider)
    }

    /// Resolve a target for a batch request in `language`, additionally checking
    /// that the provider supports batch and the requested language.
    pub fn resolve_for_batch(
        &self,
        target: &TranscriptionTargetId,
        language: Option<&str>,
    ) -> Result<Arc<dyn TranscriptionProvider>, TranscriptionError> {
        let provider = self.resolve(target)?;
        let descriptor = provider.descriptor();

        if !descriptor.capabilities.batch {
            return Err(TranscriptionError::InvalidConfiguration(format!(
                "provider `{}` does not support batch transcription",
                target.provider_id
            )));
        }

        if let Some(lang) = language {
            let supported = &descriptor.capabilities.supported_languages;
            // An empty list means "any / auto-detected". `auto` is always allowed.
            if !supported.is_empty() && lang != "auto" && !supported.iter().any(|l| l == lang) {
                return Err(TranscriptionError::UnsupportedLanguage(lang.to_string()));
            }
        }

        Ok(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::provider::TranscriptionProvider;
    use crate::transcription::types::{
        ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionModelDescriptor,
        TranscriptionRequest, TranscriptionResult,
    };
    use async_trait::async_trait;

    /// Minimal provider double used only to exercise the registry.
    struct FakeProvider {
        descriptor: ProviderDescriptor,
    }

    impl FakeProvider {
        fn cloud(
            id: &str,
            model: &str,
            languages: Vec<String>,
            batch: bool,
        ) -> Arc<dyn TranscriptionProvider> {
            Arc::new(FakeProvider {
                descriptor: ProviderDescriptor {
                    id: id.into(),
                    label: id.into(),
                    kind: ProviderKind::Cloud,
                    models: vec![TranscriptionModelDescriptor {
                        id: model.into(),
                        label: model.into(),
                    }],
                    capabilities: ProviderCapabilities {
                        batch,
                        realtime: false,
                        supported_languages: languages,
                        supports_word_timestamps: false,
                        sends_audio_off_device: true,
                    },
                    requires_credential: true,
                    privacy_url: None,
                    pricing_url: None,
                    cost_text: None,
                    consent_version: 1,
                    beta: false,
                },
            })
        }
    }

    #[async_trait]
    impl TranscriptionProvider for FakeProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            self.descriptor.clone()
        }

        async fn transcribe(
            &self,
            _request: &TranscriptionRequest,
            _api_key: &str,
        ) -> Result<TranscriptionResult, TranscriptionError> {
            unreachable!("registry tests never transcribe")
        }
    }

    fn registry_with_elevenlabs() -> TranscriptionRegistry {
        let mut registry = TranscriptionRegistry::new();
        registry.register(FakeProvider::cloud(
            "elevenlabs",
            "scribe_v2",
            vec!["fr".into(), "en".into()],
            true,
        ));
        registry
    }

    #[test]
    fn resolves_a_known_target() {
        let registry = registry_with_elevenlabs();
        let target = TranscriptionTargetId::new("elevenlabs", "scribe_v2");
        let provider = registry.resolve(&target).unwrap();
        assert_eq!(provider.descriptor().id, "elevenlabs");
    }

    #[test]
    fn unknown_provider_is_an_error_not_a_fallback() {
        let registry = registry_with_elevenlabs();
        let target = TranscriptionTargetId::new("does-not-exist", "scribe_v2");
        let err = registry.resolve(&target).unwrap_err();
        assert_eq!(err.category(), "invalid_configuration");
    }

    #[test]
    fn unknown_model_is_rejected() {
        let registry = registry_with_elevenlabs();
        let target = TranscriptionTargetId::new("elevenlabs", "scribe_v99");
        let err = registry.resolve(&target).unwrap_err();
        assert_eq!(err.category(), "invalid_configuration");
    }

    #[test]
    fn batch_unsupported_capability_is_rejected() {
        let mut registry = TranscriptionRegistry::new();
        registry.register(FakeProvider::cloud("stream-only", "m", vec![], false));
        let target = TranscriptionTargetId::new("stream-only", "m");
        let err = registry.resolve_for_batch(&target, None).unwrap_err();
        assert_eq!(err.category(), "invalid_configuration");
    }

    #[test]
    fn unsupported_language_is_rejected() {
        let registry = registry_with_elevenlabs();
        let target = TranscriptionTargetId::new("elevenlabs", "scribe_v2");
        let err = registry.resolve_for_batch(&target, Some("zz")).unwrap_err();
        assert_eq!(err.category(), "unsupported_language");
    }

    #[test]
    fn supported_language_and_auto_are_accepted() {
        let registry = registry_with_elevenlabs();
        let target = TranscriptionTargetId::new("elevenlabs", "scribe_v2");
        assert!(registry.resolve_for_batch(&target, Some("fr")).is_ok());
        assert!(registry.resolve_for_batch(&target, Some("auto")).is_ok());
        assert!(registry.resolve_for_batch(&target, None).is_ok());
    }

    #[test]
    fn empty_registry_resolves_nothing() {
        let registry = TranscriptionRegistry::new();
        let target = TranscriptionTargetId::new("elevenlabs", "scribe_v2");
        assert!(registry.resolve(&target).is_err());
        assert!(registry.descriptors().is_empty());
    }
}
