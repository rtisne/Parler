use crate::managers::model::{EngineType, ModelInfo, ModelManager};
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{get_settings, write_settings};
use crate::transcription::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionModelDescriptor,
    TranscriptionRegistry, TranscriptionTargetId,
};
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Unified target catalog for the UI: ModelManager contributes only local
/// artifacts and the registry contributes cloud providers.
#[tauri::command]
#[specta::specta]
pub async fn get_transcription_targets(
    model_manager: State<'_, Arc<ModelManager>>,
    registry: State<'_, Arc<TranscriptionRegistry>>,
) -> Result<Vec<ProviderDescriptor>, String> {
    let mut cloud = registry.descriptors();
    cloud.retain(|provider| provider.id != "local");
    cloud.sort_by(|left, right| left.label.cmp(&right.label));

    let mut targets = Vec::with_capacity(cloud.len() + 1);
    targets.push(local_descriptor(&model_manager));
    targets.extend(cloud);
    Ok(targets)
}

fn local_descriptor(model_manager: &ModelManager) -> ProviderDescriptor {
    let models = model_manager
        .get_available_models()
        .into_iter()
        .filter(|model| !matches!(model.engine_type, EngineType::InsanelyFastWhisper))
        .map(|model| TranscriptionModelDescriptor {
            id: model.id,
            label: model.name,
        })
        .collect();
    ProviderDescriptor {
        id: "local".into(),
        label: "Local".into(),
        kind: ProviderKind::Local,
        models,
        capabilities: ProviderCapabilities {
            batch: true,
            realtime: false,
            supported_languages: Vec::new(),
            supports_word_timestamps: false,
            sends_audio_off_device: false,
        },
        requires_credential: false,
        privacy_url: None,
        pricing_url: None,
        cost_text: None,
        retention_text: None,
        consent_version: 0,
        beta: false,
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_available_models(
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<Vec<ModelInfo>, String> {
    Ok(model_manager.get_available_models())
}

#[tauri::command]
#[specta::specta]
pub async fn get_model_info(
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<Option<ModelInfo>, String> {
    Ok(model_manager.get_model_info(&model_id))
}

#[tauri::command]
#[specta::specta]
pub async fn download_model(
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<(), String> {
    model_manager
        .download_model(&model_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_model(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    model_id: String,
) -> Result<(), String> {
    // If deleting the active model, unload it and clear the setting
    let settings = get_settings(&app_handle);
    if settings.selected_model == model_id {
        transcription_manager
            .unload_model()
            .map_err(|e| format!("Failed to unload model: {}", e))?;

        let mut settings = get_settings(&app_handle);
        settings.selected_model = String::new();
        if settings
            .selected_transcription_target
            .as_ref()
            .is_some_and(|target| target.provider_id == "local" && target.model_id == model_id)
        {
            settings.selected_transcription_target = None;
        }
        write_settings(&app_handle, settings);
    }

    model_manager
        .delete_model(&model_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn set_active_model(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    model_id: String,
) -> Result<(), String> {
    // Check if model exists and is available
    let model_info = model_manager
        .get_model_info(&model_id)
        .ok_or_else(|| format!("Model not found: {}", model_id))?;

    if !model_info.is_downloaded {
        return Err(format!("Model not downloaded: {}", model_id));
    }

    // Load the model in the transcription manager
    transcription_manager
        .load_model(&model_id)
        .map_err(|e| e.to_string())?;

    // Update settings
    let mut settings = get_settings(&app_handle);
    settings.selected_model = model_id.clone();
    settings.selected_transcription_target = Some(TranscriptionTargetId::new("local", model_id));
    write_settings(&app_handle, settings);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn get_current_model(app_handle: AppHandle) -> Result<String, String> {
    let settings = get_settings(&app_handle);
    Ok(settings.selected_model)
}

#[tauri::command]
#[specta::specta]
pub async fn get_transcription_model_status(
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
) -> Result<Option<String>, String> {
    Ok(transcription_manager.get_current_model())
}

#[tauri::command]
#[specta::specta]
pub async fn is_model_loading(
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
) -> Result<bool, String> {
    // Check if transcription manager has a loaded model
    let current_model = transcription_manager.get_current_model();
    Ok(current_model.is_none())
}

#[tauri::command]
#[specta::specta]
pub async fn has_any_models_available(
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<bool, String> {
    let models = model_manager.get_available_models();
    Ok(models
        .iter()
        .any(|m| m.is_downloaded && !matches!(m.engine_type, EngineType::InsanelyFastWhisper)))
}

#[tauri::command]
#[specta::specta]
pub async fn has_any_models_or_downloads(
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<bool, String> {
    let models = model_manager.get_available_models();
    Ok(models.iter().any(|m| {
        !matches!(m.engine_type, EngineType::InsanelyFastWhisper)
            && (m.is_downloaded || m.is_downloading)
    }))
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_download(
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<(), String> {
    model_manager
        .cancel_download(&model_id)
        .map_err(|e| e.to_string())
}
