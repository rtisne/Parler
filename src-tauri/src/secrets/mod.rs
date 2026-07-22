//! Abstract storage for provider API credentials.
//!
//! Provider credentials managed through this module are stored in the operating
//! system's native credential store via [`KeyringSecretStore`]. There is
//! deliberately **no** plaintext file or environment-variable fallback: if the
//! platform keyring is unavailable, operations fail loudly rather than
//! persisting a secret in the clear.
//!
//! The [`SecretStore`] trait is the seam for new provider credential paths.
//! Legacy settings are migrated separately. Tests inject the in-memory
//! [`MemorySecretStore`] instead of touching the real system keyring.

use std::fmt;

mod keyring_store;
pub use keyring_store::KeyringSecretStore;

pub mod migration;

#[cfg(test)]
mod memory_store;
#[cfg(test)]
pub use memory_store::MemorySecretStore;

/// Keyring service name under which all Parler transcription provider
/// credentials are stored.
pub const SECRET_SERVICE: &str = "parler.transcription";

/// Maximum provider identifier length accepted across platform backends.
const MAX_PROVIDER_ID_LEN: usize = 64;

/// Build the stable, provider-scoped account name for an API key entry.
///
/// The layout (`provider/<id>/api-key`) keeps providers isolated from one
/// another and is part of the persisted-data contract: changing it would
/// orphan previously stored secrets.
pub fn provider_account(provider_id: &str) -> Result<String, SecretStoreError> {
    let valid = !provider_id.is_empty()
        && provider_id.len() <= MAX_PROVIDER_ID_LEN
        && provider_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });

    if !valid {
        return Err(SecretStoreError::InvalidProviderId);
    }

    Ok(format!("provider/{provider_id}/api-key"))
}

/// Errors returned by a [`SecretStore`].
///
/// None of these variants ever carries the secret value itself, so they are
/// safe to log, format with `Debug`/`Display`, and surface to the frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStoreError {
    /// No secret is stored for the requested provider.
    NotFound,
    /// The provider identifier is empty, too long, or contains unsupported characters.
    InvalidProviderId,
    /// The supplied secret is empty or rejected by a platform size constraint.
    InvalidSecret,
    /// The platform credential store is unavailable.
    BackendUnavailable,
    /// Access to the platform credential store was denied (for example, it is locked).
    AccessDenied,
    /// An opaque platform failure occurred. Platform-provided text is never exposed.
    BackendFailure,
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretStoreError::NotFound => write!(f, "secret not found"),
            SecretStoreError::InvalidProviderId => write!(f, "invalid provider identifier"),
            SecretStoreError::InvalidSecret => write!(f, "invalid secret"),
            SecretStoreError::BackendUnavailable => write!(f, "secret backend unavailable"),
            SecretStoreError::AccessDenied => write!(f, "secret backend access denied"),
            SecretStoreError::BackendFailure => write!(f, "secret backend operation failed"),
        }
    }
}

impl std::error::Error for SecretStoreError {}

/// Read/write access to provider credentials.
///
/// Implementors must never expose the secret value through `Debug`/`Display`
/// or logs. `get_secret` exists for internal consumers (e.g. a provider client
/// building an authenticated request); it is intentionally **not** wired to any
/// Tauri command, so the frontend can never read a stored key back.
pub trait SecretStore: Send + Sync {
    /// Store (or overwrite) the secret for `provider_id`.
    fn set_secret(&self, provider_id: &str, secret: &str) -> Result<(), SecretStoreError>;

    /// Retrieve the secret for `provider_id`, or [`SecretStoreError::NotFound`]
    /// if none is stored.
    fn get_secret(&self, provider_id: &str) -> Result<String, SecretStoreError>;

    /// Delete the secret for `provider_id`. Deleting an absent secret succeeds:
    /// the desired post-condition (no secret stored) already holds.
    fn delete_secret(&self, provider_id: &str) -> Result<(), SecretStoreError>;

    /// Whether the underlying credential backend is currently reachable.
    fn backend_available(&self) -> bool;

    /// Whether a secret is currently configured for `provider_id`.
    ///
    /// Returns `Ok(false)` when no secret is stored and `Err(..)` only when the
    /// backend itself fails, so callers can distinguish "absent" from "broken".
    fn is_configured(&self, provider_id: &str) -> Result<bool, SecretStoreError> {
        match self.get_secret(provider_id) {
            Ok(_) => Ok(true),
            Err(SecretStoreError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Shared, thread-safe handle to the active secret store, stored in Tauri
/// managed state.
pub type SharedSecretStore = std::sync::Arc<dyn SecretStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_returns_stored_secret() {
        let store = MemorySecretStore::new();
        store.set_secret("elevenlabs", "sk-abc").unwrap();
        assert_eq!(store.get_secret("elevenlabs").unwrap(), "sk-abc");
    }

    #[test]
    fn get_absent_secret_is_not_found() {
        let store = MemorySecretStore::new();
        assert_eq!(
            store.get_secret("elevenlabs"),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn set_overwrites_existing_secret() {
        let store = MemorySecretStore::new();
        store.set_secret("azure", "one").unwrap();
        store.set_secret("azure", "two").unwrap();
        assert_eq!(store.get_secret("azure").unwrap(), "two");
    }

    #[test]
    fn delete_removes_secret() {
        let store = MemorySecretStore::new();
        store.set_secret("assemblyai", "key").unwrap();
        store.delete_secret("assemblyai").unwrap();
        assert_eq!(
            store.get_secret("assemblyai"),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn delete_absent_secret_is_ok() {
        let store = MemorySecretStore::new();
        assert!(store.delete_secret("nope").is_ok());
    }

    #[test]
    fn providers_are_isolated() {
        let store = MemorySecretStore::new();
        store.set_secret("elevenlabs", "eleven").unwrap();
        store.set_secret("azure", "azure-key").unwrap();

        assert_eq!(store.get_secret("elevenlabs").unwrap(), "eleven");
        assert_eq!(store.get_secret("azure").unwrap(), "azure-key");

        store.delete_secret("elevenlabs").unwrap();
        assert_eq!(
            store.get_secret("elevenlabs"),
            Err(SecretStoreError::NotFound)
        );
        // Deleting one provider must not affect another.
        assert_eq!(store.get_secret("azure").unwrap(), "azure-key");
    }

    #[test]
    fn account_names_are_stable_and_provider_scoped() {
        assert_eq!(
            provider_account("elevenlabs").unwrap(),
            "provider/elevenlabs/api-key"
        );
        assert_eq!(provider_account("azure").unwrap(), "provider/azure/api-key");
        assert_ne!(
            provider_account("a").unwrap(),
            provider_account("b").unwrap()
        );
    }

    #[test]
    fn invalid_provider_ids_are_rejected() {
        for provider_id in ["", "Azure", "with/slash", "with space", "é"] {
            assert_eq!(
                provider_account(provider_id),
                Err(SecretStoreError::InvalidProviderId)
            );
        }
        assert_eq!(
            provider_account(&"a".repeat(MAX_PROVIDER_ID_LEN + 1)),
            Err(SecretStoreError::InvalidProviderId)
        );
    }

    #[test]
    fn backend_error_is_reported_for_every_operation() {
        let store = MemorySecretStore::failing();
        assert!(matches!(
            store.set_secret("x", "y"),
            Err(SecretStoreError::BackendFailure)
        ));
        assert!(matches!(
            store.get_secret("x"),
            Err(SecretStoreError::BackendFailure)
        ));
        assert!(matches!(
            store.delete_secret("x"),
            Err(SecretStoreError::BackendFailure)
        ));
    }

    #[test]
    fn is_configured_reflects_presence() {
        let store = MemorySecretStore::new();
        assert!(!store.is_configured("p").unwrap());
        store.set_secret("p", "secret").unwrap();
        assert!(store.is_configured("p").unwrap());
    }

    #[test]
    fn is_configured_propagates_backend_error() {
        let store = MemorySecretStore::failing();
        assert!(store.is_configured("p").is_err());
    }

    #[test]
    fn backend_available_flag_is_reported() {
        assert!(MemorySecretStore::new().backend_available());
        assert!(!MemorySecretStore::unavailable().backend_available());
    }

    #[test]
    fn debug_never_exposes_secret_value() {
        let store = MemorySecretStore::new();
        store
            .set_secret("elevenlabs", "super-secret-value")
            .unwrap();
        let rendered = format!("{store:?}");
        assert!(
            !rendered.contains("super-secret-value"),
            "Debug output leaked the secret: {rendered}"
        );
    }

    #[test]
    fn error_display_and_debug_do_not_carry_secrets() {
        for err in [
            SecretStoreError::BackendUnavailable,
            SecretStoreError::AccessDenied,
            SecretStoreError::BackendFailure,
        ] {
            assert!(!format!("{err}").contains("secret-value"));
            assert!(!format!("{err:?}").contains("secret-value"));
        }
    }

    #[test]
    fn dyn_secret_store_is_usable_behind_arc() {
        let store: SharedSecretStore = std::sync::Arc::new(MemorySecretStore::new());
        store.set_secret("p", "v").unwrap();
        assert_eq!(store.get_secret("p").unwrap(), "v");
    }
}
