//! Commands for the transcription-target catalog: listing cloud providers,
//! selecting a target, and recording versioned per-provider consent.
//!
//! These never read or return a secret. Selecting a cloud target is gated on
//! (a) accepted consent at the provider's current version and (b) a configured
//! credential — enforced here so the choice cannot silently bypass either.

use std::collections::HashMap;
use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::managers::model::ModelManager;
use crate::secrets::SharedSecretStore;
use crate::settings::{get_settings, write_settings};
use crate::transcription::types::{ProviderDescriptor, ProviderKind, TranscriptionTargetId};
use crate::transcription::{TranscriptionRegistry, TranscriptionService};

/// List the registered cloud transcription providers (serializable descriptors
/// for the settings UI). Local models are listed separately by the model
/// commands; the frontend merges the two into the target catalog.
///
/// The registry also holds the local provider (so the orchestrator can resolve
/// `local/<model>` targets uniformly), so this must filter to
/// [`ProviderKind::Cloud`] explicitly. Never return the local provider here: the
/// generic cloud setup dialog persists a target without loading a model, which
/// would let a `local/<model>` selection bypass `set_active_model`'s guarantee
/// that a target is only persisted after its model actually loads.
#[tauri::command]
#[specta::specta]
pub async fn get_cloud_transcription_providers(
    registry: State<'_, Arc<TranscriptionRegistry>>,
) -> Result<Vec<ProviderDescriptor>, String> {
    Ok(cloud_only(registry.descriptors()))
}

/// Keep only cloud descriptors from a registry snapshot. Extracted as a plain
/// function so the filter is unit-testable without a Tauri app context.
fn cloud_only(descriptors: Vec<ProviderDescriptor>) -> Vec<ProviderDescriptor> {
    descriptors
        .into_iter()
        .filter(|descriptor| descriptor.kind == ProviderKind::Cloud)
        .collect()
}

/// Validate the credential already stored in the keyring without uploading audio.
#[tauri::command]
#[specta::specta]
pub async fn test_cloud_provider_connection(
    service: State<'_, Arc<TranscriptionService>>,
    provider_id: String,
) -> Result<(), String> {
    service
        .test_connection(&provider_id)
        .await
        .map_err(|error| error.category().to_string())
}

/// The currently selected explicit transcription target, if any.
#[tauri::command]
#[specta::specta]
pub async fn get_selected_transcription_target(
    app_handle: AppHandle,
) -> Result<Option<TranscriptionTargetId>, String> {
    Ok(get_settings(&app_handle).selected_transcription_target)
}

/// Select (or clear, with `None`) the transcription target. Selecting a cloud
/// target requires prior consent and a configured credential; otherwise a
/// stable error category is returned for the frontend to localize. There is no
/// implicit fallback: an unknown/unsupported target is rejected.
#[tauri::command]
#[specta::specta]
pub async fn set_selected_transcription_target(
    app_handle: AppHandle,
    registry: State<'_, Arc<TranscriptionRegistry>>,
    model_manager: State<'_, Arc<ModelManager>>,
    secret_store: State<'_, SharedSecretStore>,
    target: Option<TranscriptionTargetId>,
) -> Result<(), String> {
    let mut settings = get_settings(&app_handle);

    if let Some(ref t) = target {
        // A target registered as a provider must be validated. Local models are
        // not in the registry and pass through to the existing model path.
        if let Some(provider) = registry.get(&t.provider_id) {
            let descriptor = provider.descriptor();
            let model_known = descriptor.models.iter().any(|m| m.id == t.model_id);
            if !model_known {
                return Err("invalid_configuration".to_string());
            }
            if descriptor.kind == ProviderKind::Cloud {
                let accepted = settings
                    .cloud_provider_consents
                    .get(&t.provider_id)
                    .copied()
                    .unwrap_or(0);
                if accepted < descriptor.consent_version {
                    return Err("consent_required".to_string());
                }
                if descriptor.requires_credential
                    && !secret_store.is_configured(&t.provider_id).unwrap_or(false)
                {
                    return Err("missing_credential".to_string());
                }
            }
        } else if t.provider_id == "local" {
            if !model_manager
                .get_available_models()
                .iter()
                .any(|model| model.id == t.model_id)
            {
                return Err("invalid_configuration".to_string());
            }
        } else {
            return Err("invalid_configuration".to_string());
        }
    }

    if let Some(ref selected) = target {
        if selected.provider_id == "local" {
            settings.selected_model = selected.model_id.clone();
        }
    }
    settings.selected_transcription_target = target;
    write_settings(&app_handle, settings);
    Ok(())
}

/// Record acceptance of a cloud provider's consent notice at `version`.
#[tauri::command]
#[specta::specta]
pub async fn set_cloud_provider_consent(
    app_handle: AppHandle,
    registry: State<'_, Arc<TranscriptionRegistry>>,
    provider_id: String,
    version: u32,
) -> Result<(), String> {
    let provider = registry
        .get(&provider_id)
        .ok_or_else(|| "invalid_configuration".to_string())?;
    let descriptor = provider.descriptor();
    if descriptor.kind != ProviderKind::Cloud || version != descriptor.consent_version {
        return Err("invalid_configuration".to_string());
    }
    let mut settings = get_settings(&app_handle);
    settings
        .cloud_provider_consents
        .insert(provider_id, version);
    write_settings(&app_handle, settings);
    Ok(())
}

/// Revoke consent for a cloud provider and, if it is the selected target, clear
/// the selection so no cloud transcription can run without renewed consent.
#[tauri::command]
#[specta::specta]
pub async fn revoke_cloud_provider_consent(
    app_handle: AppHandle,
    provider_id: String,
) -> Result<(), String> {
    let mut settings = get_settings(&app_handle);
    settings.cloud_provider_consents.remove(&provider_id);
    if let Some(ref t) = settings.selected_transcription_target {
        if t.provider_id == provider_id {
            settings.selected_transcription_target = None;
        }
    }
    write_settings(&app_handle, settings);
    Ok(())
}

/// Accepted consent versions keyed by provider id.
#[tauri::command]
#[specta::specta]
pub async fn get_cloud_provider_consents(
    app_handle: AppHandle,
) -> Result<HashMap<String, u32>, String> {
    Ok(get_settings(&app_handle).cloud_provider_consents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::types::{ProviderCapabilities, TranscriptionModelDescriptor};

    fn descriptor(id: &str, kind: ProviderKind) -> ProviderDescriptor {
        ProviderDescriptor {
            id: id.into(),
            label: id.into(),
            kind,
            models: vec![TranscriptionModelDescriptor {
                id: "model".into(),
                label: "Model".into(),
            }],
            capabilities: ProviderCapabilities {
                batch: true,
                realtime: false,
                supported_languages: vec![],
                supports_word_timestamps: false,
                sends_audio_off_device: kind == ProviderKind::Cloud,
            },
            requires_credential: kind == ProviderKind::Cloud,
            privacy_url: None,
            pricing_url: None,
            cost_text: None,
            retention_text: None,
            consent_version: 0,
            beta: false,
        }
    }

    #[test]
    fn cloud_only_excludes_the_local_provider() {
        let descriptors = vec![
            descriptor("local", ProviderKind::Local),
            descriptor("elevenlabs", ProviderKind::Cloud),
            descriptor("gemini", ProviderKind::Cloud),
        ];

        let cloud = cloud_only(descriptors);

        assert_eq!(cloud.len(), 2);
        assert!(cloud.iter().all(|d| d.kind == ProviderKind::Cloud));
        assert!(!cloud.iter().any(|d| d.id == "local"));
    }

    #[test]
    fn cloud_only_of_local_only_registry_is_empty() {
        let descriptors = vec![descriptor("local", ProviderKind::Local)];
        assert!(cloud_only(descriptors).is_empty());
    }
}
