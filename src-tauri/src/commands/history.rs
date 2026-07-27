use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::transcription::{RestoreOutcome, TicketGuard, TranscriptionManager};
use crate::transcription::{TranscriptionRequest, TranscriptionService, TranscriptionTargetId};
use log::warn;
use std::sync::Arc;
use tauri::{AppHandle, State};

/// Maps a local-model restore attempt to the command's result. A restoration
/// failure must reach the caller fail-closed instead of being reduced to a
/// warning and a silent success: the local model state is now unknown, and
/// the caller (and the user) need to know that, even though the reprocessed
/// text itself was already saved. `Superseded` is not a failure -- it means
/// a newer explicit selection (or another concurrent reprocess) legitimately
/// changed the model after this one finished, and correctly was not
/// clobbered.
fn finalize_reprocess(new_text: String, restore_outcome: RestoreOutcome) -> Result<String, String> {
    match restore_outcome {
        RestoreOutcome::Restored | RestoreOutcome::Superseded => Ok(new_text),
        RestoreOutcome::Failed(error) => {
            warn!(
                "Failed to restore the previous local model state: {}",
                error
            );
            Err("Failed to restore the previous local model state".to_string())
        }
    }
}

/// Combines a primary failure (inference error or history-update error) with
/// whatever the restore attempt made of it. A restore failure must never be
/// silently dropped just because something else already failed first -- both
/// need to reach the caller.
fn finalize_reprocess_error(
    primary_error: String,
    restore_outcome: RestoreOutcome,
) -> Result<String, String> {
    match restore_outcome {
        RestoreOutcome::Failed(restore_error) => {
            warn!(
                "Failed to restore the previous local model state after a reprocess error: {}",
                restore_error
            );
            Err(format!(
                "{primary_error}; additionally failed to restore the previous local model state"
            ))
        }
        RestoreOutcome::Restored | RestoreOutcome::Superseded => Err(primary_error),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
) -> Result<Vec<HistoryEntry>, String> {
    history_manager
        .get_history_entries()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_history_entry_saved(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .toggle_saved_status(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_audio_file_path(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_name: String,
) -> Result<String, String> {
    let path = history_manager.get_audio_file_path(&file_name);
    path.to_str()
        .ok_or_else(|| "Invalid file path".to_string())
        .map(|s| s.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .delete_entry(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn update_history_limit(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    limit: usize,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.history_limit = limit;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn reprocess_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    id: i64,
    model_id: String,
) -> Result<String, String> {
    let entry = history_manager
        .get_entry_by_id(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "History entry not found".to_string())?;

    let audio_path = history_manager.get_audio_file_path(&entry.file_name);
    if !audio_path.exists() {
        return Err("Audio file not found".to_string());
    }

    let samples = crate::audio_toolkit::load_wav_file(&audio_path).map_err(|e| e.to_string())?;

    let reprocessed_model_name = transcription_manager.get_model_name(&model_id);

    // `transcribe_local_ticketed` targets the local provider directly
    // (reprocess always does -- see the hardcoded "local" target below),
    // switching to `model_id` and opening a `RestoreTicket` atomically
    // (still under the local provider's operation gate) right after this
    // call's own switch/inference finished -- capturing whatever was loaded
    // before as the restore baseline. The ticket is returned regardless of
    // whether transcription itself succeeded, and `TicketGuard` guarantees
    // it gets resolved on every exit path below (inference error, history
    // write error, or success), so a temporary reprocess model can never be
    // left stranded loaded.
    let (transcribe_result, ticket) =
        transcription_manager.transcribe_local_ticketed(&model_id, samples);
    let mut ticket_guard = TicketGuard::new((**transcription_manager).clone(), ticket);

    let new_text = match transcribe_result {
        Ok(text) => text,
        Err(error) => {
            let restore_outcome = ticket_guard.resolve();
            return finalize_reprocess_error(error.category().to_string(), restore_outcome);
        }
    };

    if let Err(e) =
        history_manager.update_transcription_text(id, &new_text, reprocessed_model_name.as_deref())
    {
        let restore_outcome = ticket_guard.resolve();
        return finalize_reprocess_error(e.to_string(), restore_outcome);
    }

    let restore_outcome = ticket_guard.resolve();
    finalize_reprocess(new_text, restore_outcome)
}

/// Retry a failed cloud history entry with its original explicit provider/model.
/// This never falls back to a local model and never pastes automatically.
#[tauri::command]
#[specta::specta]
pub async fn retry_cloud_history_entry(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_service: State<'_, Arc<TranscriptionService>>,
    id: i64,
) -> Result<String, String> {
    let entry = history_manager
        .get_entry_by_id(id)
        .await
        .map_err(|_| "history_read_failed".to_string())?
        .ok_or_else(|| "history_entry_not_found".to_string())?;
    let model_name = entry
        .model_name
        .as_deref()
        .ok_or_else(|| "cloud_target_missing".to_string())?;
    let (provider_id, model_id) = model_name
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .ok_or_else(|| "cloud_target_invalid".to_string())?;
    if provider_id == "local" {
        return Err("cloud_target_required".to_string());
    }

    let audio_path = history_manager.get_audio_file_path(&entry.file_name);
    if !audio_path.exists() {
        return Err("audio_file_not_found".to_string());
    }
    let samples = crate::audio_toolkit::load_wav_file(&audio_path)
        .map_err(|_| "audio_read_failed".to_string())?;
    let settings = crate::settings::get_settings(&app);
    let accepted_consent_version = settings
        .cloud_provider_consents
        .get(provider_id)
        .copied()
        .unwrap_or_default();
    let language = match settings.selected_language.as_str() {
        "auto" => None,
        "zh-Hans" | "zh-Hant" => Some("zh".to_string()),
        value => Some(value.to_string()),
    };
    let request = TranscriptionRequest {
        audio_16khz_mono: samples,
        language,
        custom_words: settings.custom_words.clone(),
    };
    let target = TranscriptionTargetId::new(provider_id, model_id);
    let result = transcription_service
        .transcribe_batch(&target, accepted_consent_version, &request)
        .await
        .map_err(|error| error.category().to_string())?;
    let corrected = if settings.custom_words.is_empty() {
        result.text
    } else {
        crate::audio_toolkit::apply_custom_words(
            &result.text,
            &settings.custom_words,
            settings.word_correction_threshold,
        )
    };
    let text = crate::audio_toolkit::filter_transcription_output(&corrected);
    history_manager
        .update_transcription_text(id, &text, Some(model_name))
        .map_err(|_| "history_update_failed".to_string())?;
    Ok(text)
}

#[tauri::command]
#[specta::specta]
pub async fn update_recording_retention_period(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    period: String,
) -> Result<(), String> {
    use crate::settings::RecordingRetentionPeriod;

    let retention_period = match period.as_str() {
        "never" => RecordingRetentionPeriod::Never,
        "preserve_limit" => RecordingRetentionPeriod::PreserveLimit,
        "days3" => RecordingRetentionPeriod::Days3,
        "weeks2" => RecordingRetentionPeriod::Weeks2,
        "months3" => RecordingRetentionPeriod::Months3,
        _ => return Err(format!("Invalid retention period: {}", period)),
    };

    let mut settings = crate::settings::get_settings(&app);
    settings.recording_retention_period = retention_period;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod restoration_tests {
    use super::{finalize_reprocess, finalize_reprocess_error, RestoreOutcome};

    #[test]
    fn restored_outcome_returns_the_new_text() {
        assert_eq!(
            finalize_reprocess("hello".to_string(), RestoreOutcome::Restored),
            Ok("hello".to_string())
        );
    }

    #[test]
    fn superseded_outcome_is_not_a_failure() {
        // A newer explicit selection (or another reprocess) legitimately
        // changed the model after ours finished; that must not surface as
        // an error to the caller even though the restore was skipped.
        assert_eq!(
            finalize_reprocess("hello".to_string(), RestoreOutcome::Superseded),
            Ok("hello".to_string())
        );
    }

    #[test]
    fn failed_outcome_is_signaled_fail_closed_to_the_caller() {
        // Must never be reduced to a warning + silent success: the caller
        // needs to know the local model state is now unknown.
        let result = finalize_reprocess(
            "hello".to_string(),
            RestoreOutcome::Failed("load failed".to_string()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn primary_error_surfaces_when_restore_succeeds() {
        // e.g. an inference error: the transcription itself failed, but the
        // restore (attempted regardless) worked fine -- only the original
        // error should reach the caller.
        let result =
            finalize_reprocess_error("inference failed".to_string(), RestoreOutcome::Restored);
        assert_eq!(result, Err("inference failed".to_string()));
    }

    #[test]
    fn primary_error_surfaces_when_restore_is_superseded() {
        let result = finalize_reprocess_error(
            "history write failed".to_string(),
            RestoreOutcome::Superseded,
        );
        assert_eq!(result, Err("history write failed".to_string()));
    }

    #[test]
    fn a_restore_failure_is_never_dropped_even_when_something_else_already_failed() {
        // Both the primary error (e.g. inference) and the restore failure
        // are real problems; neither may be silently swallowed by the other.
        let result = finalize_reprocess_error(
            "inference failed".to_string(),
            RestoreOutcome::Failed("load failed".to_string()),
        );
        let error = result.expect_err("must surface an error");
        assert!(error.contains("inference failed"));
        assert!(error.contains("restore"));
    }
}
