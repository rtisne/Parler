//! [`SecretStore`] backed by the operating system's native credential store.

use std::fmt;
use std::sync::{Mutex, MutexGuard};

use keyring::{Entry, Error as KeyringError};

use super::{provider_account, SecretStore, SecretStoreError, SECRET_SERVICE};

/// Reserved account used only to probe backend availability. It is never
/// written, so a healthy backend reports it as absent (`NoEntry`).
const PROBE_ACCOUNT: &str = "__backend_probe__";

/// [`SecretStore`] backed by the platform keyring: macOS Keychain, Windows
/// Credential Manager, or the Linux Secret Service.
///
/// There is intentionally no file or environment-variable fallback. If the
/// platform keyring is unavailable the operations fail with
/// [`SecretStoreError::BackendUnavailable`] rather than persisting the secret
/// in clear text. Accesses are serialized because some platform backends do not
/// reliably support concurrent operations.
#[derive(Default)]
pub struct KeyringSecretStore {
    access: Mutex<()>,
}

impl KeyringSecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(&self, provider_id: &str) -> Result<Entry, SecretStoreError> {
        Entry::new(SECRET_SERVICE, &provider_account(provider_id)?).map_err(map_entry_error)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, SecretStoreError> {
        self.access
            .lock()
            .map_err(|_| SecretStoreError::BackendFailure)
    }
}

impl fmt::Debug for KeyringSecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyringSecretStore")
    }
}

fn map_keyring_error(error: KeyringError) -> SecretStoreError {
    match error {
        KeyringError::NoEntry => SecretStoreError::NotFound,
        KeyringError::NoStorageAccess(_) => SecretStoreError::AccessDenied,
        KeyringError::PlatformFailure(_) => SecretStoreError::BackendUnavailable,
        _ => SecretStoreError::BackendFailure,
    }
}

fn map_entry_error(error: KeyringError) -> SecretStoreError {
    match error {
        KeyringError::TooLong(_, _) | KeyringError::Invalid(_, _) => {
            SecretStoreError::InvalidProviderId
        }
        error => map_keyring_error(error),
    }
}

fn map_set_error(error: KeyringError) -> SecretStoreError {
    match error {
        KeyringError::TooLong(_, _) | KeyringError::Invalid(_, _) => {
            SecretStoreError::InvalidSecret
        }
        error => map_keyring_error(error),
    }
}

impl SecretStore for KeyringSecretStore {
    fn set_secret(&self, provider_id: &str, secret: &str) -> Result<(), SecretStoreError> {
        let _guard = self.lock()?;
        self.entry(provider_id)?
            .set_password(secret)
            .map_err(map_set_error)
    }

    fn get_secret(&self, provider_id: &str) -> Result<String, SecretStoreError> {
        let _guard = self.lock()?;
        match self.entry(provider_id)?.get_password() {
            Ok(secret) => Ok(secret),
            Err(KeyringError::NoEntry) => Err(SecretStoreError::NotFound),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn delete_secret(&self, provider_id: &str) -> Result<(), SecretStoreError> {
        let _guard = self.lock()?;
        match self.entry(provider_id)?.delete_credential() {
            Ok(()) => Ok(()),
            // Deleting an absent credential is a no-op success: the desired
            // post-condition (no secret stored) already holds.
            Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn backend_available(&self) -> bool {
        let Ok(_guard) = self.lock() else {
            return false;
        };
        // Probe with a reserved account we never write. A reachable backend
        // either returns a value or definitively reports its absence
        // (`NoEntry`); any other error means the backend is unusable.
        match Entry::new(SECRET_SERVICE, PROBE_ACCOUNT) {
            Ok(entry) => matches!(entry.get_password(), Ok(_) | Err(KeyringError::NoEntry)),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_a_safe_marker() {
        // The handle stores only a lock: Debug can never leak a secret.
        assert_eq!(
            format!("{:?}", KeyringSecretStore::new()),
            "KeyringSecretStore"
        );
    }

    #[test]
    fn entry_uses_stable_service_and_account() {
        // Constructing an entry must not require a live backend; only that the
        // service/account naming contract stays intact.
        let store = KeyringSecretStore::new();
        assert!(store.entry("elevenlabs").is_ok());
        assert_eq!(SECRET_SERVICE, "parler.transcription");
        assert_eq!(
            provider_account("elevenlabs").unwrap(),
            "provider/elevenlabs/api-key"
        );
    }

    #[test]
    fn keyring_errors_are_mapped_to_closed_safe_variants() {
        assert_eq!(
            map_keyring_error(KeyringError::NoEntry),
            SecretStoreError::NotFound
        );
        assert_eq!(
            map_keyring_error(KeyringError::NoStorageAccess(Box::new(
                std::io::Error::other("sensitive platform detail")
            ))),
            SecretStoreError::AccessDenied
        );
        assert_eq!(
            map_keyring_error(KeyringError::PlatformFailure(Box::new(
                std::io::Error::other("sensitive platform detail")
            ))),
            SecretStoreError::BackendUnavailable
        );
        assert_eq!(
            map_entry_error(KeyringError::Invalid(
                "account".to_string(),
                "sensitive platform detail".to_string()
            )),
            SecretStoreError::InvalidProviderId
        );
        assert_eq!(
            map_set_error(KeyringError::TooLong("password".to_string(), 42)),
            SecretStoreError::InvalidSecret
        );
        assert!(!SecretStoreError::BackendFailure
            .to_string()
            .contains("sensitive platform detail"));
    }

    /// End-to-end round trip against the real platform keyring. Ignored by
    /// default because CI/headless environments have no Secret Service; run
    /// explicitly with `cargo test -- --ignored` on a desktop session.
    #[test]
    #[ignore]
    fn real_keyring_round_trip() {
        let store = KeyringSecretStore::new();
        let provider = "parler-test-provider";

        store.set_secret(provider, "round-trip-secret").unwrap();
        assert_eq!(store.get_secret(provider).unwrap(), "round-trip-secret");

        store.delete_secret(provider).unwrap();
        assert_eq!(store.get_secret(provider), Err(SecretStoreError::NotFound));
        // Deleting again must remain a success.
        store.delete_secret(provider).unwrap();
    }
}
