//! Write-only Tauri commands for provider API credentials.
//!
//! The frontend can set a credential, delete it, and query whether one is
//! configured — but there is deliberately **no** command that returns the
//! stored secret value. The only response type ([`CredentialStatus`]) carries
//! booleans, never the key itself.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager};

use crate::secrets::{SecretStore, SecretStoreError, SharedSecretStore};

/// Write-only status of a provider credential returned to the frontend.
///
/// It intentionally exposes no `value`/`secret`/`api_key` field: the UI learns
/// only whether a key is configured and whether the OS keyring is reachable.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CredentialStatus {
    /// Whether a secret is currently stored for the provider.
    pub configured: bool,
    /// Whether the underlying OS credential backend is reachable.
    pub backend_available: bool,
}

/// Convert a store error into a frontend/log-safe string.
///
/// [`SecretStoreError`] never carries the secret value, so this cannot leak it.
fn store_error_to_string(err: SecretStoreError) -> String {
    err.to_string()
}

/// Store a credential after the same fail-closed validation every write path
/// (the generic command, the Gemini command, ...) must apply. Blank/whitespace
/// values are rejected here as a defensive check for callers; [`SecretStore::set_secret`]
/// also enforces this at the storage layer so no backend can ever persist one.
pub(crate) fn set_credential(
    store: &dyn SecretStore,
    provider_id: &str,
    secret: &str,
) -> Result<(), SecretStoreError> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(SecretStoreError::InvalidSecret);
    }
    store.set_secret(provider_id, secret)
}

pub(crate) fn delete_credential(
    store: &dyn SecretStore,
    provider_id: &str,
) -> Result<(), SecretStoreError> {
    store.delete_secret(provider_id)
}

fn credential_status(
    store: &dyn SecretStore,
    provider_id: &str,
) -> Result<CredentialStatus, SecretStoreError> {
    match store.is_configured(provider_id) {
        Ok(configured) => Ok(CredentialStatus {
            configured,
            backend_available: true,
        }),
        Err(SecretStoreError::InvalidProviderId) => Err(SecretStoreError::InvalidProviderId),
        Err(_) => Ok(CredentialStatus {
            configured: false,
            backend_available: false,
        }),
    }
}

/// Store (or overwrite) the API key for `provider_id` in the system keyring.
#[tauri::command]
#[specta::specta]
pub fn set_provider_credential(
    app: AppHandle,
    provider_id: String,
    secret: String,
) -> Result<(), String> {
    let store = app.state::<SharedSecretStore>();
    set_credential(store.inner().as_ref(), &provider_id, &secret).map_err(store_error_to_string)
}

/// Delete any stored API key for `provider_id`. Succeeds even if none exists.
#[tauri::command]
#[specta::specta]
pub fn delete_provider_credential(app: AppHandle, provider_id: String) -> Result<(), String> {
    let store = app.state::<SharedSecretStore>();
    delete_credential(store.inner().as_ref(), &provider_id).map_err(store_error_to_string)?;

    // Credential removal and target deactivation are one backend operation from
    // the UI's perspective: a target must never remain active without its key.
    let mut settings = crate::settings::get_settings(&app);
    if settings
        .selected_transcription_target
        .as_ref()
        .is_some_and(|target| target.provider_id == provider_id)
    {
        settings.selected_transcription_target = None;
        crate::settings::write_settings(&app, settings);
    }
    Ok(())
}

/// Report whether a credential is configured for `provider_id` and whether the
/// keyring backend is reachable. Never returns the secret value.
#[tauri::command]
#[specta::specta]
pub fn get_provider_credential_status(
    app: AppHandle,
    provider_id: String,
) -> Result<CredentialStatus, String> {
    let store = app.state::<SharedSecretStore>();
    credential_status(store.inner().as_ref(), &provider_id).map_err(store_error_to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretStore;

    #[test]
    fn set_then_status_reports_configured() {
        let store = MemorySecretStore::new();
        set_credential(&store, "elevenlabs", "sk-123").unwrap();

        let status = credential_status(&store, "elevenlabs").unwrap();
        assert!(status.configured);
        assert!(status.backend_available);
    }

    #[test]
    fn status_for_unknown_provider_is_unconfigured() {
        let store = MemorySecretStore::new();
        let status = credential_status(&store, "azure").unwrap();
        assert!(!status.configured);
        assert!(status.backend_available);
    }

    #[test]
    fn empty_secret_is_rejected() {
        let store = MemorySecretStore::new();
        assert!(set_credential(&store, "p", "").is_err());
        assert!(!credential_status(&store, "p").unwrap().configured);
    }

    #[test]
    fn whitespace_only_secret_is_rejected() {
        let store = MemorySecretStore::new();
        assert_eq!(
            set_credential(&store, "p", "   \t  "),
            Err(SecretStoreError::InvalidSecret)
        );
        assert!(!credential_status(&store, "p").unwrap().configured);
    }

    #[test]
    fn secret_with_surrounding_whitespace_is_stored_trimmed() {
        let store = MemorySecretStore::new();
        set_credential(&store, "p", "  sk-abc  ").unwrap();
        assert_eq!(store.get_secret("p").unwrap(), "sk-abc");
    }

    #[test]
    fn delete_clears_configured_flag() {
        let store = MemorySecretStore::new();
        set_credential(&store, "p", "secret").unwrap();
        delete_credential(&store, "p").unwrap();
        assert!(!credential_status(&store, "p").unwrap().configured);
    }

    #[test]
    fn delete_unknown_provider_is_ok() {
        let store = MemorySecretStore::new();
        assert!(delete_credential(&store, "never-set").is_ok());
    }

    #[test]
    fn status_reports_unavailable_backend() {
        let store = MemorySecretStore::unavailable();
        let status = credential_status(&store, "p").unwrap();
        assert!(!status.configured);
        assert!(!status.backend_available);
    }

    #[test]
    fn status_reports_backend_failure_as_unavailable() {
        let store = MemorySecretStore::failing();
        let status = credential_status(&store, "p").unwrap();
        assert!(!status.configured);
        assert!(!status.backend_available);
    }

    #[test]
    fn invalid_provider_id_is_rejected() {
        let store = MemorySecretStore::new();
        assert_eq!(
            set_credential(&store, "../invalid", "secret"),
            Err(SecretStoreError::InvalidProviderId)
        );
        assert!(matches!(
            credential_status(&store, "../invalid"),
            Err(SecretStoreError::InvalidProviderId)
        ));
    }

    #[test]
    fn backend_failure_surfaces_error_without_leaking_secret() {
        let store = MemorySecretStore::failing();
        let err = set_credential(&store, "p", "top-secret-value").unwrap_err();
        let message = store_error_to_string(err);
        assert!(
            !message.contains("top-secret-value"),
            "error leaked secret: {message}"
        );
    }

    /// Static guarantee that the only response type is write-only: it must not
    /// serialize any field that could carry the secret back to the frontend.
    #[test]
    fn credential_status_serializes_no_secret_field() {
        let json = serde_json::to_string(&CredentialStatus {
            configured: true,
            backend_available: true,
        })
        .unwrap();

        assert_eq!(json, r#"{"configured":true,"backend_available":true}"#);
        for forbidden in ["secret", "api_key", "apiKey", "value", "password", "key"] {
            assert!(
                !json.contains(forbidden),
                "response type exposes forbidden field `{forbidden}`: {json}"
            );
        }
    }
}
