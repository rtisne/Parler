//! Commands for the transcription-target catalog: listing cloud providers,
//! selecting a target, and recording versioned per-provider consent.
//!
//! These never read or return a secret. Selecting a cloud target is gated on
//! (a) accepted consent at the provider's current version and (b) a configured
//! credential — enforced here so the choice cannot silently bypass either.

use std::collections::HashMap;
use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::secrets::SharedSecretStore;
use crate::settings::{get_settings, write_settings};
use crate::transcription::types::{ProviderDescriptor, ProviderKind, TranscriptionTargetId};
use crate::transcription::TranscriptionRegistry;

/// List the registered cloud transcription providers (serializable descriptors
/// for the settings UI). Local models are listed separately by the model
/// commands; the frontend merges the two into the target catalog.
#[tauri::command]
#[specta::specta]
pub async fn get_cloud_transcription_providers(
    registry: State<'_, Arc<TranscriptionRegistry>>,
) -> Result<Vec<ProviderDescriptor>, String> {
    Ok(registry.descriptors())
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
    provider_id: String,
    version: u32,
) -> Result<(), String> {
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
