pub mod audio;
pub mod gemini;
pub mod hardware;
pub mod history;
pub mod insanely_fast_whisper;
pub mod models;
pub mod providers;
pub mod secrets;
pub mod transcription;

use crate::settings::{get_settings, write_settings, AppSettings, LogLevel};
use crate::utils::cancel_current_operation;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
#[specta::specta]
pub fn cancel_operation(app: AppHandle) {
    cancel_current_operation(&app);
}

#[tauri::command]
#[specta::specta]
pub fn toggle_pause(app: AppHandle) -> bool {
    let audio_manager =
        app.state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>();
    if !audio_manager.is_recording() {
        return false;
    }
    let paused = audio_manager.toggle_pause();
    crate::overlay::emit_recording_paused(&app, paused);
    paused
}

#[tauri::command]
#[specta::specta]
pub fn get_app_dir_path(app: AppHandle) -> Result<String, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    Ok(app_data_dir.to_string_lossy().to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_app_settings(app: AppHandle) -> Result<AppSettings, String> {
    let mut settings = get_settings(&app);
    // Legacy values may remain on disk when keyring migration is blocked, but
    // credential material must never cross the Tauri boundary.
    crate::secrets::migration::clear_legacy_plaintext(&mut settings);
    Ok(settings)
}

#[tauri::command]
#[specta::specta]
pub fn get_default_settings() -> Result<AppSettings, String> {
    Ok(crate::settings::get_default_settings())
}

#[tauri::command]
#[specta::specta]
pub fn get_log_dir_path(app: AppHandle) -> Result<String, String> {
    let log_dir = app
        .path()
        .app_log_dir()
        .map_err(|e| format!("Failed to get log directory: {}", e))?;

    Ok(log_dir.to_string_lossy().to_string())
}

#[specta::specta]
#[tauri::command]
pub fn set_log_level(app: AppHandle, level: LogLevel) -> Result<(), String> {
    let tauri_log_level: tauri_plugin_log::LogLevel = level.into();
    let log_level: log::Level = tauri_log_level.into();
    // Update the file log level atomic so the filter picks up the new level
    crate::FILE_LOG_LEVEL.store(
        log_level.to_level_filter() as u8,
        std::sync::atomic::Ordering::Relaxed,
    );

    let mut settings = get_settings(&app);
    settings.log_level = level;
    write_settings(&app, settings);

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_recordings_folder(app: AppHandle) -> Result<(), String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    let recordings_dir = app_data_dir.join("recordings");

    let path = recordings_dir.to_string_lossy().as_ref().to_string();
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| format!("Failed to open recordings folder: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> Result<(), String> {
    let log_dir = app
        .path()
        .app_log_dir()
        .map_err(|e| format!("Failed to get log directory: {}", e))?;

    let path = log_dir.to_string_lossy().as_ref().to_string();
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| format!("Failed to open log directory: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_app_data_dir(app: AppHandle) -> Result<(), String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    let path = app_data_dir.to_string_lossy().as_ref().to_string();
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|e| format!("Failed to open app data directory: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn export_settings(app: AppHandle, path: String) -> Result<(), String> {
    // Never export credential material: strip secrets before serializing.
    let settings = get_settings(&app).export_snapshot();
    let json = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write file: {}", e))?;
    log::info!("Settings exported to {}", path);
    Ok(())
}

fn sanitize_imported_settings(settings: &mut AppSettings) {
    crate::secrets::migration::clear_legacy_plaintext(settings);
    settings.cloud_provider_consents.clear();

    // Legacy cloud pseudo-models are privileged configuration too: leaving one
    // here would let the one-time target migration recreate a cloud selection.
    if settings.selected_model == "gemini-api" {
        settings.selected_model.clear();
    }
    if settings.long_audio_model.as_deref() == Some("gemini-api") {
        settings.long_audio_model = None;
    }
    if settings
        .selected_transcription_target
        .as_ref()
        .is_some_and(|target| target.provider_id != "local")
    {
        settings.selected_transcription_target = None;
    }
    if settings
        .long_audio_target
        .as_ref()
        .is_some_and(|target| target.provider_id != "local")
    {
        settings.long_audio_target = None;
    }
    if settings.selected_transcription_target.is_none() && !settings.selected_model.is_empty() {
        settings.selected_transcription_target =
            Some(crate::transcription::TranscriptionTargetId::new(
                "local",
                settings.selected_model.clone(),
            ));
    }
    settings.transcription_target_migration_version = 1;
    settings.secret_store_migration_version = crate::secrets::migration::CURRENT_MIGRATION_VERSION;
}

#[specta::specta]
#[tauri::command]
pub fn import_settings(app: AppHandle, path: String) -> Result<(), String> {
    let json = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {}", e))?;
    let mut settings: AppSettings =
        serde_json::from_str(&json).map_err(|e| format!("Invalid settings file: {}", e))?;
    // Imported files are configuration only. Never persist credential material,
    // including values from legacy exports. Consent and cloud activation are
    // likewise local, interactive decisions and cannot be imported.
    sanitize_imported_settings(&mut settings);
    write_settings(&app, settings);
    log::info!("Settings imported from {}", path);
    Ok(())
}

/// Check if Apple Intelligence is available on this device.
/// Called by the frontend when the user selects Apple Intelligence provider.
#[specta::specta]
#[tauri::command]
pub fn check_apple_intelligence_available() -> bool {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        crate::apple_intelligence::check_apple_intelligence_availability()
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        false
    }
}

/// Try to initialize Enigo (keyboard/mouse simulation).
/// On macOS, this will return an error if accessibility permissions are not granted.
#[specta::specta]
#[tauri::command]
pub fn initialize_enigo(app: AppHandle) -> Result<(), String> {
    use crate::input::EnigoState;

    // Check if already initialized
    if app.try_state::<EnigoState>().is_some() {
        log::debug!("Enigo already initialized");
        return Ok(());
    }

    // Try to initialize
    match EnigoState::new() {
        Ok(enigo_state) => {
            app.manage(enigo_state);
            log::info!("Enigo initialized successfully after permission grant");
            Ok(())
        }
        Err(e) => {
            if cfg!(target_os = "macos") {
                log::warn!(
                    "Failed to initialize Enigo: {} (accessibility permissions may not be granted)",
                    e
                );
            } else {
                log::warn!("Failed to initialize Enigo: {}", e);
            }
            Err(format!("Failed to initialize input system: {}", e))
        }
    }
}

/// Marker state to track if shortcuts have been initialized.
pub struct ShortcutsInitialized;

/// Initialize keyboard shortcuts.
/// On macOS, this should be called after accessibility permissions are granted.
/// This is idempotent - calling it multiple times is safe.
#[specta::specta]
#[tauri::command]
pub fn initialize_shortcuts(app: AppHandle) -> Result<(), String> {
    // Check if already initialized
    if app.try_state::<ShortcutsInitialized>().is_some() {
        log::debug!("Shortcuts already initialized");
        return Ok(());
    }

    // Initialize shortcuts
    crate::shortcut::init_shortcuts(&app);

    // Mark as initialized
    app.manage(ShortcutsInitialized);

    log::info!("Shortcuts initialized successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::TranscriptionTargetId;

    #[test]
    fn imported_cloud_activation_and_consent_are_stripped() {
        let mut settings = crate::settings::get_default_settings();
        settings.gemini_api_key = Some("legacy-secret".into());
        settings
            .post_process_api_keys
            .insert("openai".into(), "legacy-secret-2".into());
        settings
            .cloud_provider_consents
            .insert("elevenlabs".into(), 99);
        settings.selected_model = "gemini-api".into();
        settings.long_audio_model = Some("gemini-api".into());
        settings.long_audio_target = Some(TranscriptionTargetId::new("gemini", "legacy"));
        settings.selected_transcription_target =
            Some(TranscriptionTargetId::new("elevenlabs", "scribe_v2"));

        sanitize_imported_settings(&mut settings);

        assert!(settings.gemini_api_key.is_none());
        assert!(settings
            .post_process_api_keys
            .values()
            .all(String::is_empty));
        assert!(settings.cloud_provider_consents.is_empty());
        assert!(settings.selected_transcription_target.is_none());
        assert!(settings.selected_model.is_empty());
        assert!(settings.long_audio_model.is_none());
        assert!(settings.long_audio_target.is_none());
        assert_eq!(settings.transcription_target_migration_version, 1);
    }

    #[test]
    fn imported_local_target_is_preserved() {
        let mut settings = crate::settings::get_default_settings();
        let target = TranscriptionTargetId::new("local", "whisper-small");
        settings.selected_transcription_target = Some(target.clone());

        sanitize_imported_settings(&mut settings);

        assert_eq!(settings.selected_transcription_target, Some(target));
    }
}
