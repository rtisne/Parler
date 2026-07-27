use crate::commands::secrets::{delete_credential, set_credential};
use crate::secrets::SharedSecretStore;
use crate::transcription::{GeminiProvider, TranscriptionRegistry, TranscriptionTargetId};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

/// Whether deleting the Gemini credential should also clear the currently
/// selected transcription target, so a target is never left selected
/// without the credential it needs. Mirrors the generic
/// `delete_provider_credential` command's behavior (`commands/secrets.rs`)
/// for other providers -- the Gemini-specific command must not be a special
/// case that leaves a dangling active target behind.
fn gemini_delete_clears_target(selected: Option<&TranscriptionTargetId>) -> bool {
    selected.is_some_and(|target| target.provider_id == "gemini")
}

#[tauri::command]
#[specta::specta]
pub fn change_gemini_api_key_setting(app: AppHandle, api_key: String) -> Result<(), String> {
    let store = app.state::<SharedSecretStore>();
    // Same fail-closed validation as the generic credential command: a
    // whitespace-only key is treated as absent (clear), never stored.
    let is_delete = api_key.trim().is_empty();
    if is_delete {
        delete_credential(store.inner().as_ref(), "gemini").map_err(|err| err.to_string())?;
    } else {
        set_credential(store.inner().as_ref(), "gemini", &api_key)
            .map_err(|err| err.to_string())?;
    }

    // Never retain a newly supplied key in settings. Clearing this legacy field
    // is safe only after the keyring write/delete above succeeded.
    let mut settings = crate::settings::get_settings(&app);
    settings.gemini_api_key = None;
    if is_delete && gemini_delete_clears_target(settings.selected_transcription_target.as_ref()) {
        settings.selected_transcription_target = None;
    }
    crate::settings::write_settings(&app, settings);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_key_deletion_clears_the_target_when_gemini_is_active() {
        let target = TranscriptionTargetId::new("gemini", "gemini-2.5-flash");
        assert!(gemini_delete_clears_target(Some(&target)));
    }

    #[test]
    fn blank_key_deletion_leaves_a_different_active_target_untouched() {
        let target = TranscriptionTargetId::new("elevenlabs", "scribe_v2");
        assert!(!gemini_delete_clears_target(Some(&target)));
    }

    #[test]
    fn blank_key_deletion_is_a_no_op_when_nothing_is_selected() {
        assert!(!gemini_delete_clears_target(None));
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_gemini_model_setting(
    app: AppHandle,
    registry: State<'_, Arc<TranscriptionRegistry>>,
    model: String,
) -> Result<(), String> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("invalid_configuration".into());
    }

    let mut settings = crate::settings::get_settings(&app);
    settings.gemini_model = model.clone();
    if settings
        .selected_transcription_target
        .as_ref()
        .is_some_and(|target| target.provider_id == "gemini")
    {
        settings.selected_transcription_target =
            Some(TranscriptionTargetId::new("gemini", model.clone()));
    }
    crate::settings::write_settings(&app, settings);
    registry.register(Arc::new(GeminiProvider::with_model(model)));
    Ok(())
}
