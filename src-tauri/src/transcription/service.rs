//! Batch transcription orchestration shared by every cloud provider.
//!
//! The service is the only layer allowed to read a provider credential. It
//! resolves the exact selected target, validates versioned consent, retrieves
//! the key from the OS credential store, and invokes that provider once. It
//! never retries or falls back to another target.

use std::sync::Arc;

use crate::secrets::{SecretStoreError, SharedSecretStore};

use super::{
    ProviderKind, TranscriptionError, TranscriptionRegistry, TranscriptionRequest,
    TranscriptionResult, TranscriptionTargetId,
};

#[derive(Clone)]
pub struct TranscriptionService {
    registry: Arc<TranscriptionRegistry>,
    secrets: SharedSecretStore,
}

impl TranscriptionService {
    pub fn new(registry: Arc<TranscriptionRegistry>, secrets: SharedSecretStore) -> Self {
        Self { registry, secrets }
    }

    pub async fn test_connection(&self, provider_id: &str) -> Result<(), TranscriptionError> {
        let provider = self
            .registry
            .get(provider_id)
            .ok_or_else(|| TranscriptionError::InvalidConfiguration("unknown provider".into()))?;
        let descriptor = provider.descriptor();
        if descriptor.kind != ProviderKind::Cloud || !descriptor.requires_credential {
            return Err(TranscriptionError::InvalidConfiguration(
                "provider does not support credential testing".into(),
            ));
        }
        let secret = self
            .secrets
            .get_secret(provider_id)
            .map_err(|error| match error {
                SecretStoreError::NotFound => TranscriptionError::MissingCredential,
                _ => TranscriptionError::ProviderUnavailable,
            })?;
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(TranscriptionError::MissingCredential);
        }
        provider.test_connection(secret).await
    }

    pub async fn transcribe_batch(
        &self,
        target: &TranscriptionTargetId,
        accepted_consent_version: u32,
        request: &TranscriptionRequest,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        let provider = self
            .registry
            .resolve_for_batch(target, request.language.as_deref())?;
        let descriptor = provider.descriptor();

        if descriptor.kind == ProviderKind::Cloud
            && accepted_consent_version < descriptor.consent_version
        {
            return Err(TranscriptionError::ConsentRequired);
        }

        let credential = if descriptor.requires_credential {
            let secret =
                self.secrets
                    .get_secret(&target.provider_id)
                    .map_err(|error| match error {
                        SecretStoreError::NotFound => TranscriptionError::MissingCredential,
                        _ => TranscriptionError::ProviderUnavailable,
                    })?;
            let secret = secret.trim();
            if secret.is_empty() {
                return Err(TranscriptionError::MissingCredential);
            }
            secret.to_string()
        } else {
            String::new()
        };

        provider
            .transcribe(&target.model_id, request, &credential)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::secrets::{MemorySecretStore, SecretStore};
    use crate::transcription::{
        ProviderCapabilities, ProviderDescriptor, TranscriptionLatency,
        TranscriptionModelDescriptor, TranscriptionProvider,
    };

    use super::*;

    struct FakeProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TranscriptionProvider for FakeProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                id: "cloud".into(),
                label: "Cloud".into(),
                kind: ProviderKind::Cloud,
                models: vec![TranscriptionModelDescriptor {
                    id: "model".into(),
                    label: "Model".into(),
                }],
                capabilities: ProviderCapabilities {
                    batch: true,
                    realtime: false,
                    supported_languages: vec![],
                    supports_word_timestamps: false,
                    sends_audio_off_device: true,
                },
                requires_credential: true,
                privacy_url: None,
                pricing_url: None,
                cost_text: None,
                retention_text: None,
                consent_version: 2,
                beta: true,
            }
        }

        async fn transcribe(
            &self,
            model_id: &str,
            _request: &TranscriptionRequest,
            api_key: &str,
        ) -> Result<TranscriptionResult, TranscriptionError> {
            assert_eq!(model_id, "model");
            assert_eq!(api_key, "credential");
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TranscriptionResult {
                text: "bonjour".into(),
                provider_id: "cloud".into(),
                model_id: "model".into(),
                detected_language: Some("fr".into()),
                latency: TranscriptionLatency::default(),
            })
        }
    }

    fn request() -> TranscriptionRequest {
        TranscriptionRequest {
            audio_16khz_mono: vec![0.0],
            language: Some("fr".into()),
            custom_words: vec![],
        }
    }

    fn setup(with_secret: bool) -> (TranscriptionService, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = TranscriptionRegistry::new();
        registry.register(Arc::new(FakeProvider {
            calls: calls.clone(),
        }));
        let secrets = Arc::new(MemorySecretStore::new());
        if with_secret {
            secrets.set_secret("cloud", "credential").unwrap();
        }
        (
            TranscriptionService::new(Arc::new(registry), secrets),
            calls,
        )
    }

    #[test]
    fn invokes_exact_provider_once_when_consent_and_credential_are_valid() {
        let (service, calls) = setup(true);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(service.transcribe_batch(
                &TranscriptionTargetId::new("cloud", "model"),
                2,
                &request(),
            ))
            .unwrap();
        assert_eq!(result.text, "bonjour");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_consent_fails_before_provider_call() {
        let (service, calls) = setup(true);
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(service.transcribe_batch(
                &TranscriptionTargetId::new("cloud", "model"),
                1,
                &request(),
            ))
            .err()
            .expect("stale consent must fail");
        assert_eq!(error, TranscriptionError::ConsentRequired);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_credential_fails_without_fallback_or_provider_call() {
        let (service, calls) = setup(false);
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(service.transcribe_batch(
                &TranscriptionTargetId::new("cloud", "model"),
                2,
                &request(),
            ))
            .err()
            .expect("missing credential must fail");
        assert_eq!(error, TranscriptionError::MissingCredential);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn whitespace_credential_fails_without_provider_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = TranscriptionRegistry::new();
        registry.register(Arc::new(FakeProvider {
            calls: calls.clone(),
        }));
        let secrets = Arc::new(MemorySecretStore::new());
        // `set_secret` now rejects blank/whitespace values outright, so this
        // simulates a value written before that guard existed (e.g. by an
        // older build or a legacy migration path) to prove the service layer
        // is still fail-closed against it, as defense in depth.
        secrets.seed_raw("cloud", "   \t  ");
        let service = TranscriptionService::new(Arc::new(registry), secrets);

        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(service.transcribe_batch(
                &TranscriptionTargetId::new("cloud", "model"),
                2,
                &request(),
            ))
            .err()
            .expect("whitespace credential must fail");
        assert_eq!(error, TranscriptionError::MissingCredential);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
