use tauri::AppHandle;

#[tauri::command]
#[specta::specta]
pub fn change_insanely_fast_whisper_model_setting(
    app: AppHandle,
    model: String,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.insanely_fast_whisper_model = if model.is_empty() { None } else { Some(model) };
    crate::settings::write_settings(&app, settings);
    Ok(())
}
