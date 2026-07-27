use crate::audio_toolkit::{apply_custom_words, filter_transcription_output};
use crate::hardware_detection::get_hardware_capabilities;
use crate::managers::model::{EngineType, ModelManager};
use crate::settings::{get_settings, ModelUnloadTimeout};
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use serde::Serialize;
use std::collections::VecDeque;
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter};
use transcribe_rs::{
    engines::{
        moonshine::{
            ModelVariant, MoonshineEngine, MoonshineModelParams, MoonshineStreamingEngine,
            StreamingModelParams,
        },
        parakeet::{
            ParakeetEngine, ParakeetInferenceParams, ParakeetModelParams, TimestampGranularity,
        },
        sense_voice::{
            Language as SenseVoiceLanguage, SenseVoiceEngine, SenseVoiceInferenceParams,
            SenseVoiceModelParams,
        },
        whisper::{WhisperEngine, WhisperInferenceParams, WhisperModelParams},
    },
    TranscriptionEngine,
};

use crate::transcription::{
    ProviderCapabilities, ProviderDescriptor, ProviderKind, TranscriptionError,
    TranscriptionLatency, TranscriptionModelDescriptor, TranscriptionProvider,
    TranscriptionRequest, TranscriptionResult,
};

pub const PROVIDER_ID: &str = "local";

/// Build the local catalog from ModelManager's on-device artifacts only.
pub fn descriptor(model_manager: &ModelManager) -> ProviderDescriptor {
    let models = model_manager
        .get_available_models()
        .into_iter()
        .map(|model| TranscriptionModelDescriptor {
            id: model.id,
            label: model.name,
        })
        .collect();
    ProviderDescriptor {
        id: PROVIDER_ID.into(),
        label: "Local".into(),
        kind: ProviderKind::Local,
        models,
        capabilities: ProviderCapabilities {
            batch: true,
            realtime: false,
            supported_languages: vec![],
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

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

enum LoadedEngine {
    Whisper(WhisperEngine),
    Parakeet(ParakeetEngine),
    Moonshine(MoonshineEngine),
    MoonshineStreaming(MoonshineStreamingEngine),
    SenseVoice(SenseVoiceEngine),
    InsanelyFastWhisper,
}

#[derive(Default)]
struct LocalOperationGate(Mutex<()>);

impl LocalOperationGate {
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Result of attempting to restore a previously captured [`RestoreTicket`].
#[derive(Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The slot now matches (or was brought back to) the requested state.
    Restored,
    /// A genuinely newer explicit selection landed after the ticket was
    /// opened. Refusing to restore here is the correct, non-failure
    /// outcome: overwriting it would silently discard a more recent,
    /// legitimate choice.
    Superseded,
    /// The restore was attempted (nothing superseded it) but the underlying
    /// load/unload failed.
    Failed(String),
}

/// Distinguishes a genuine, permanent change of ground truth (an explicit
/// `set_active_model`/manual load or unload, engine-panic recovery, or an
/// idle-timeout auto-unload) from a temporary operation's own working
/// switch or restore (reprocess, long-audio fallback). Only `Explicit`
/// mutations bump [`VersionedModelSlot::explicit_epoch`] -- that's what lets
/// a deferred restore tell a legitimate newer user choice (never clobber)
/// apart from another temporary operation merely still being in flight
/// (wait your turn, then still restore).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationKind {
    Explicit,
    Temporary,
}

/// A temporary operation's (reprocess, long-audio fallback) claim on
/// eventually restoring the model slot back to whatever was loaded right
/// before its own switch. Tickets nest like a stack: the most recently
/// opened one must be the first to restore, which is what prevents two
/// concurrent temporary operations from stranding each other's intermediate
/// model (see [`VersionedModelSlot::restore_ticket`]).
pub struct RestoreTicket {
    id: u64,
    restore_to: Option<String>,
    explicit_epoch_at_open: u64,
}

/// Bumps `counter` by 1, panicking instead of silently wrapping if it would
/// overflow `u64`. At one bump per nanosecond this is ~584 years away, so
/// this is a defensive invariant, not a realistic failure mode: a silent
/// wrap here would let a stale generation/epoch/ticket id become
/// indistinguishable from a current one (an ABA hazard in exactly the
/// restore-safety checks this module exists to guarantee).
fn checked_bump(counter: &AtomicU64) -> u64 {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |g| g.checked_add(1))
        .unwrap_or_else(|g| {
            panic!("model slot counter overflowed u64 (was {g}) -- this must never happen")
        })
}

/// Tracks which model is current, gated so a switch/restore transaction can
/// never race a concurrent mutation, plus the bookkeeping that lets
/// temporary operations (reprocess, long-audio fallback) unwind safely and
/// in the right order:
/// - `generation` is bumped by every mutation.
/// - `explicit_epoch` is bumped only by genuine, permanent changes (see
///   [`MutationKind`]); a ticket opened before a newer bump here must never
///   clobber it.
/// - `tickets` is a LIFO stack of outstanding temporary-operation restore
///   claims; a ticket may only actually restore once it is the most
///   recently opened (the top), so nested temporary operations always
///   unwind in reverse order regardless of which one finishes first.
struct VersionedModelSlot {
    gate: LocalOperationGate,
    current: Mutex<Option<String>>,
    generation: AtomicU64,
    explicit_epoch: AtomicU64,
    next_ticket: AtomicU64,
    tickets: Mutex<VecDeque<u64>>,
    ticket_progress: Condvar,
}

impl VersionedModelSlot {
    fn new() -> Self {
        Self {
            gate: LocalOperationGate::default(),
            current: Mutex::new(None),
            generation: AtomicU64::new(0),
            explicit_epoch: AtomicU64::new(0),
            next_ticket: AtomicU64::new(0),
            tickets: Mutex::new(VecDeque::new()),
            ticket_progress: Condvar::new(),
        }
    }

    /// The single gate serializing every model mutation with inference.
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.gate.lock()
    }

    fn current(&self) -> Option<String> {
        self.current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Caller must already hold the gate.
    fn set(&self, kind: MutationKind, model_id: Option<String>) {
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = model_id;
        checked_bump(&self.generation);
        if kind == MutationKind::Explicit {
            checked_bump(&self.explicit_epoch);
        }
    }

    /// Caller must already hold the gate. Registers a new restore ticket
    /// with `restore_to` as its baseline and the slot's current explicit
    /// epoch, so a later `restore_ticket` call can tell whether a genuine
    /// newer explicit selection has landed since.
    fn open_ticket(&self, restore_to: Option<String>) -> RestoreTicket {
        let id = checked_bump(&self.next_ticket);
        self.tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(id);
        RestoreTicket {
            id,
            restore_to,
            explicit_epoch_at_open: self.explicit_epoch.load(Ordering::SeqCst),
        }
    }

    /// Caller must already hold the gate. Used when the switch a ticket was
    /// opened for never actually happened (e.g. the requested model failed
    /// to load): there is nothing to restore, so just drop the bookkeeping
    /// and let anything waiting behind it re-check its own position.
    fn abandon_ticket(&self, ticket: &RestoreTicket) {
        self.close_ticket(ticket);
    }

    fn close_ticket(&self, ticket: &RestoreTicket) {
        {
            let mut tickets = self
                .tickets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if tickets.back() == Some(&ticket.id) {
                tickets.pop_back();
            } else {
                // Defensive: should not normally happen since callers only
                // close their own ticket once it is the top, but never leave
                // a stale id stranded in the middle of the stack.
                tickets.retain(|id| *id != ticket.id);
            }
        }
        self.ticket_progress.notify_all();
    }

    fn is_ticket_top(&self, ticket: &RestoreTicket) -> bool {
        self.tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .back()
            == Some(&ticket.id)
    }

    /// Bounded wait for `close_ticket`/`abandon_ticket` to make progress.
    /// Every ticket is required to eventually be resolved by its owner (see
    /// `TicketGuard` in `managers/transcription.rs`), so this should always
    /// be woken well before the timeout; the timeout is only a safety net
    /// against a future bug leaving one unresolved forever.
    fn wait_for_ticket_progress(&self) {
        let guard = self
            .tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self
            .ticket_progress
            .wait_timeout(guard, Duration::from_secs(30));
    }

    /// Restore `ticket`'s baseline. Blocks until `ticket` is the top of the
    /// restore stack (so nested temporary operations unwind strictly in
    /// reverse order), unless a genuinely newer explicit selection has
    /// landed since the ticket was opened, in which case it is correctly
    /// abandoned (`Superseded`) rather than clobbering that newer choice.
    /// `apply` performs the actual load/unload for the `Temporary` mutation
    /// this produces on success.
    fn restore_ticket(
        &self,
        ticket: RestoreTicket,
        apply: impl Fn(Option<&str>) -> Result<(), String>,
    ) -> RestoreOutcome {
        loop {
            {
                let _guard = self.lock();
                if self.explicit_epoch.load(Ordering::SeqCst) != ticket.explicit_epoch_at_open {
                    self.close_ticket(&ticket);
                    return RestoreOutcome::Superseded;
                }
                if self.is_ticket_top(&ticket) {
                    let outcome = if self.current() == ticket.restore_to {
                        RestoreOutcome::Restored
                    } else {
                        match apply(ticket.restore_to.as_deref()) {
                            Ok(()) => RestoreOutcome::Restored,
                            Err(error) => RestoreOutcome::Failed(error),
                        }
                    };
                    self.close_ticket(&ticket);
                    return outcome;
                }
            }
            self.wait_for_ticket_progress();
        }
    }
}

/// Owns the idle watcher's `JoinHandle`. It is the single, sole owner of
/// shutdown/join for that thread: `join()` takes the handle out (so a second
/// call, or a concurrent one, is a no-op) and drops the guard *before*
/// blocking on `.join()`, so nothing that thread might still need to acquire
/// (e.g. this same mutex, reached from a value living inside it) can ever be
/// stuck waiting on a lock this call is itself holding.
struct WatcherHandle(Mutex<Option<thread::JoinHandle<()>>>);

impl WatcherHandle {
    fn empty() -> Self {
        Self(Mutex::new(None))
    }

    fn install(&self, handle: thread::JoinHandle<()>) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
    }

    fn join(&self) {
        let handle = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

        if let Some(handle) = handle {
            if handle.thread().id() == thread::current().id() {
                // We *are* the watched thread: it just became the sole
                // remaining strong owner of `LocalProviderState` (via its
                // `Weak::upgrade()`) and is now dropping this `WatcherHandle`
                // from within its own call stack. Blocking on `handle.join()`
                // here would deadlock forever -- this thread cannot finish
                // running this very call until it returns. Detach instead:
                // the OS reclaims the thread the moment this call stack
                // actually returns, which is imminent since we're already
                // unwinding through this thread's own teardown.
                warn!("Idle watcher would join its own thread; detaching instead");
                return;
            }
            if let Err(e) = handle.join() {
                warn!("Failed to join idle watcher thread: {:?}", e);
            } else {
                debug!("Idle watcher thread joined successfully");
            }
        }
    }

    #[cfg(test)]
    fn probe_lock(&self) {
        let _guard = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.join();
    }
}

/// Owns every piece of local-provider state. Exists behind a single `Arc` so
/// shutdown runs exactly once, when the last [`LocalProvider`] handle goes
/// away -- never once per cheap clone (see [`LocalProvider`]).
pub struct LocalProviderState {
    engine: Mutex<Option<LoadedEngine>>,
    model_manager: Arc<ModelManager>,
    app_handle: AppHandle,
    model_slot: VersionedModelSlot,
    last_activity: AtomicU64,
    watcher_handle: WatcherHandle,
    is_loading: Mutex<bool>,
    loading_condvar: Condvar,
}

/// Cheaply cloneable handle to the local transcription provider. Every clone
/// shares one [`LocalProviderState`] through the inner `Arc`; only the last
/// one dropped actually tears anything down. Field/method access on
/// `LocalProvider` transparently reaches `LocalProviderState` via [`Deref`].
#[derive(Clone)]
pub struct LocalProvider(Arc<LocalProviderState>);

impl Deref for LocalProvider {
    type Target = LocalProviderState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

impl LocalProvider {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Result<Self> {
        let state = Arc::new(LocalProviderState {
            engine: Mutex::new(None),
            model_manager,
            app_handle: app_handle.clone(),
            model_slot: VersionedModelSlot::new(),
            last_activity: AtomicU64::new(now_ms()),
            watcher_handle: WatcherHandle::empty(),
            is_loading: Mutex::new(false),
            loading_condvar: Condvar::new(),
        });

        // Start the idle watcher. It holds only a `Weak` reference -- never a
        // full `LocalProvider`/`Arc<LocalProviderState>` clone -- so it can
        // never keep the state alive on its own. That's what makes shutdown
        // deterministic: `LocalProviderState::drop` (which joins this very
        // thread) only ever runs when the last *real* owner goes away, and
        // the watcher notices via a failed `upgrade()` on its next wake
        // rather than via a shared flag it could itself be responsible for
        // setting.
        {
            let weak_state = Arc::downgrade(&state);
            let app_handle_cloned = app_handle.clone();
            let handle = thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(10)); // Check every 10 seconds

                    let Some(state) = weak_state.upgrade() else {
                        break;
                    };

                    let settings = get_settings(&app_handle_cloned);
                    let timeout_seconds = settings.model_unload_timeout.to_seconds();

                    if let Some(limit_seconds) = timeout_seconds {
                        // Skip polling-based unloading for immediate timeout since it's handled directly in transcribe()
                        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately {
                            continue;
                        }

                        let last = state.last_activity.load(Ordering::Relaxed);
                        let now_ms = now_ms();

                        if now_ms.saturating_sub(last) > limit_seconds * 1000 {
                            // idle -> unload
                            if state.is_model_loaded() {
                                let unload_start = std::time::Instant::now();
                                debug!("Starting to unload model due to inactivity");

                                if let Ok(()) = state.unload_model() {
                                    let _ = app_handle_cloned.emit(
                                        "model-state-changed",
                                        ModelStateEvent {
                                            event_type: "unloaded".to_string(),
                                            model_id: None,
                                            model_name: None,
                                            error: None,
                                        },
                                    );
                                    let unload_duration = unload_start.elapsed();
                                    debug!(
                                        "Model unloaded due to inactivity (took {}ms)",
                                        unload_duration.as_millis()
                                    );
                                }
                            }
                        }
                    }
                }
                debug!("Idle watcher thread shutting down gracefully");
            });
            state.watcher_handle.install(handle);
        }

        let provider = LocalProvider(state);

        // Preload model if enabled in settings
        let settings = get_settings(app_handle);
        if settings.preload_model_on_startup && !settings.selected_model.is_empty() {
            info!("Preloading model on startup: {}", settings.selected_model);
            provider.initiate_model_load();
        }

        Ok(provider)
    }

    /// Kicks off the model loading in a background thread if it's not already
    /// loaded. The spawned thread holds a full strong clone (unlike the idle
    /// watcher): it must keep the state alive until the load finishes, and
    /// -- because `Drop` now only runs once, when the last strong reference
    /// goes away -- doing so no longer risks tearing anything down early.
    pub fn initiate_model_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading || self.is_model_loaded() {
            return;
        }

        *is_loading = true;
        let self_clone = self.clone();
        thread::spawn(move || {
            let settings = get_settings(&self_clone.app_handle);
            if let Err(e) = self_clone.load_model(&settings.selected_model) {
                error!("Failed to load model: {}", e);
            }
            let mut is_loading = self_clone.is_loading.lock().unwrap();
            *is_loading = false;
            self_clone.loading_condvar.notify_all();
        });
    }
}

impl LocalProviderState {
    /// Lock the engine mutex, recovering from poison if a previous transcription panicked.
    fn lock_engine(&self) -> MutexGuard<'_, Option<LoadedEngine>> {
        self.engine.lock().unwrap_or_else(|poisoned| {
            warn!("Engine mutex was poisoned by a previous panic, recovering");
            poisoned.into_inner()
        })
    }

    fn lock_operation(&self) -> MutexGuard<'_, ()> {
        self.model_slot.lock()
    }

    fn wait_for_background_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        while *is_loading {
            is_loading = self.loading_condvar.wait(is_loading).unwrap();
        }
    }

    pub fn is_model_loaded(&self) -> bool {
        let engine = self.lock_engine();
        engine.is_some()
    }

    pub fn unload_model(&self) -> Result<()> {
        let _operation = self.lock_operation();
        self.unload_model_inner(MutationKind::Explicit)
    }

    fn unload_model_inner(&self, kind: MutationKind) -> Result<()> {
        let unload_start = std::time::Instant::now();
        debug!("Starting to unload model");

        {
            let mut engine = self.lock_engine();
            if let Some(ref mut loaded_engine) = *engine {
                match loaded_engine {
                    LoadedEngine::Whisper(ref mut e) => e.unload_model(),
                    LoadedEngine::Parakeet(ref mut e) => e.unload_model(),
                    LoadedEngine::Moonshine(ref mut e) => e.unload_model(),
                    LoadedEngine::MoonshineStreaming(ref mut e) => e.unload_model(),
                    LoadedEngine::SenseVoice(ref mut e) => e.unload_model(),
                    LoadedEngine::InsanelyFastWhisper => {}
                }
            }
            *engine = None; // Drop the engine to free memory
        }
        self.model_slot.set(kind, None);

        // Emit unloaded event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: None,
            },
        );

        let unload_duration = unload_start.elapsed();
        debug!(
            "Model unloaded manually (took {}ms)",
            unload_duration.as_millis()
        );
        Ok(())
    }

    /// Unloads the model immediately if the setting is enabled and the model is loaded
    pub fn maybe_unload_immediately(&self, context: &str) {
        let _operation = self.lock_operation();
        self.maybe_unload_immediately_inner(context);
    }

    fn maybe_unload_immediately_inner(&self, context: &str) {
        let settings = get_settings(&self.app_handle);
        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately
            && self.is_model_loaded()
        {
            info!("Immediately unloading model after {}", context);
            if let Err(e) = self.unload_model_inner(MutationKind::Explicit) {
                warn!("Failed to immediately unload model: {}", e);
            }
        }
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        let _operation = self.lock_operation();
        self.load_model_inner(model_id, MutationKind::Explicit)
    }

    /// Switches to `model_id` (if it isn't already current) and opens a
    /// [`RestoreTicket`] atomically with that switch (still holding the
    /// gate), capturing whatever was current *before* the switch as the
    /// baseline to restore later via [`Self::restore_ticket`]. Used by
    /// temporary operations (long-audio fallback) that need to switch the
    /// model for one call and reliably put it back afterward. On failure the
    /// ticket is closed internally (nothing to restore) and the caller gets
    /// no ticket to resolve.
    pub fn begin_temporary_switch(
        &self,
        model_id: &str,
    ) -> Result<RestoreTicket, TranscriptionError> {
        let _operation = self.lock_operation();
        let baseline = self.get_current_model();
        let ticket = self.model_slot.open_ticket(baseline.clone());
        if baseline.as_deref() != Some(model_id) {
            if self
                .load_model_inner(model_id, MutationKind::Temporary)
                .is_err()
            {
                self.model_slot.abandon_ticket(&ticket);
                return Err(TranscriptionError::InvalidConfiguration(
                    "requested local model could not be loaded".into(),
                ));
            }
        }
        Ok(ticket)
    }

    /// Restore a [`RestoreTicket`] previously opened by
    /// [`Self::begin_temporary_switch`] or [`Self::transcribe_target_ticketed`].
    /// See [`VersionedModelSlot::restore_ticket`] for the exact semantics.
    pub fn restore_ticket(&self, ticket: RestoreTicket) -> RestoreOutcome {
        self.model_slot
            .restore_ticket(ticket, |target| match target {
                Some(id) => self
                    .load_model_inner(id, MutationKind::Temporary)
                    .map_err(|e| e.to_string()),
                None => self
                    .unload_model_inner(MutationKind::Temporary)
                    .map_err(|e| e.to_string()),
            })
    }

    fn load_model_inner(&self, model_id: &str, kind: MutationKind) -> Result<()> {
        let load_start = std::time::Instant::now();
        debug!("Starting to load model: {}", model_id);

        // Emit loading started event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_started".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: None,
                error: None,
            },
        );

        let model_info = self
            .model_manager
            .get_model_info(model_id)
            .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        if !model_info.is_downloaded {
            let error_msg = "Model not downloaded";
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
            return Err(anyhow::anyhow!(error_msg));
        }

        let model_path = if matches!(model_info.engine_type, EngineType::InsanelyFastWhisper) {
            std::path::PathBuf::new()
        } else {
            self.model_manager.get_model_path(model_id)?
        };

        // Create appropriate engine based on model type
        let loaded_engine = match model_info.engine_type {
            EngineType::Whisper => {
                let mut engine = WhisperEngine::new();

                engine
                    .load_model_with_params(&model_path, WhisperModelParams::default())
                    .map_err(|e| {
                        let error_msg = format!("Failed to load whisper model {}: {}", model_id, e);
                        let _ = self.app_handle.emit(
                            "model-state-changed",
                            ModelStateEvent {
                                event_type: "loading_failed".to_string(),
                                model_id: Some(model_id.to_string()),
                                model_name: Some(model_info.name.clone()),
                                error: Some(error_msg.clone()),
                            },
                        );
                        anyhow::anyhow!(error_msg)
                    })?;

                info!("Loaded Whisper model");
                LoadedEngine::Whisper(engine)
            }
            EngineType::Parakeet => {
                let mut engine = ParakeetEngine::new();
                engine
                    .load_model_with_params(&model_path, ParakeetModelParams::int8())
                    .map_err(|e| {
                        let error_msg =
                            format!("Failed to load parakeet model {}: {}", model_id, e);
                        let _ = self.app_handle.emit(
                            "model-state-changed",
                            ModelStateEvent {
                                event_type: "loading_failed".to_string(),
                                model_id: Some(model_id.to_string()),
                                model_name: Some(model_info.name.clone()),
                                error: Some(error_msg.clone()),
                            },
                        );
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::Parakeet(engine)
            }
            EngineType::Moonshine => {
                let mut engine = MoonshineEngine::new();
                engine
                    .load_model_with_params(
                        &model_path,
                        MoonshineModelParams::variant(ModelVariant::Base),
                    )
                    .map_err(|e| {
                        let error_msg =
                            format!("Failed to load moonshine model {}: {}", model_id, e);
                        let _ = self.app_handle.emit(
                            "model-state-changed",
                            ModelStateEvent {
                                event_type: "loading_failed".to_string(),
                                model_id: Some(model_id.to_string()),
                                model_name: Some(model_info.name.clone()),
                                error: Some(error_msg.clone()),
                            },
                        );
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::Moonshine(engine)
            }
            EngineType::MoonshineStreaming => {
                let mut engine = MoonshineStreamingEngine::new();
                engine
                    .load_model_with_params(&model_path, StreamingModelParams::default())
                    .map_err(|e| {
                        let error_msg = format!(
                            "Failed to load moonshine streaming model {}: {}",
                            model_id, e
                        );
                        let _ = self.app_handle.emit(
                            "model-state-changed",
                            ModelStateEvent {
                                event_type: "loading_failed".to_string(),
                                model_id: Some(model_id.to_string()),
                                model_name: Some(model_info.name.clone()),
                                error: Some(error_msg.clone()),
                            },
                        );
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::MoonshineStreaming(engine)
            }
            EngineType::SenseVoice => {
                let mut engine = SenseVoiceEngine::new();
                engine
                    .load_model_with_params(&model_path, SenseVoiceModelParams::int8())
                    .map_err(|e| {
                        let error_msg =
                            format!("Failed to load SenseVoice model {}: {}", model_id, e);
                        let _ = self.app_handle.emit(
                            "model-state-changed",
                            ModelStateEvent {
                                event_type: "loading_failed".to_string(),
                                model_id: Some(model_id.to_string()),
                                model_name: Some(model_info.name.clone()),
                                error: Some(error_msg.clone()),
                            },
                        );
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::SenseVoice(engine)
            }

            EngineType::InsanelyFastWhisper => {
                // Check that the insanely-fast-whisper CLI is available in PATH
                let check = std::process::Command::new("insanely-fast-whisper")
                    .arg("--help")
                    .output();
                if check.is_err() {
                    let error_msg = "insanely-fast-whisper is not installed. Install it with: pip install insanely-fast-whisper";
                    let _ = self.app_handle.emit(
                        "model-state-changed",
                        ModelStateEvent {
                            event_type: "loading_failed".to_string(),
                            model_id: Some(model_id.to_string()),
                            model_name: Some(model_info.name.clone()),
                            error: Some(error_msg.to_string()),
                        },
                    );
                    return Err(anyhow::anyhow!(error_msg));
                }
                LoadedEngine::InsanelyFastWhisper
            }
        };

        // Update the current engine and model ID
        {
            let mut engine = self.lock_engine();
            *engine = Some(loaded_engine);
        }
        self.model_slot.set(kind, Some(model_id.to_string()));

        // Emit loading completed event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_completed".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: Some(model_info.name.clone()),
                error: None,
            },
        );

        let load_duration = load_start.elapsed();
        debug!(
            "Successfully loaded transcription model: {} (took {}ms)",
            model_id,
            load_duration.as_millis()
        );
        Ok(())
    }

    pub fn get_current_model(&self) -> Option<String> {
        self.model_slot.current()
    }

    pub fn get_current_model_name(&self) -> Option<String> {
        let model_id = self.get_current_model()?;
        self.get_model_name(&model_id)
    }

    pub fn get_model_name(&self, model_id: &str) -> Option<String> {
        self.model_manager
            .get_model_info(model_id)
            .map(|info| info.name)
    }

    pub fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        self.wait_for_background_load();
        let result = {
            let _operation = self.lock_operation();
            let result = self.transcribe_inner(audio);
            self.maybe_unload_immediately_inner("transcription");
            result
        };
        result
    }

    fn transcribe_inner(&self, audio: Vec<f32>) -> Result<String> {
        // Update last activity timestamp
        self.last_activity.store(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            Ordering::Relaxed,
        );

        let st = std::time::Instant::now();

        debug!("Audio vector length: {}", audio.len());

        if audio.is_empty() {
            debug!("Empty audio vector");
            return Ok(String::new());
        }

        // Verify that the operation owns a loaded engine.
        {
            let engine_guard = self.lock_engine();
            if engine_guard.is_none() {
                return Err(anyhow::anyhow!("Model is not loaded for transcription."));
            }
        }

        // Get current settings for configuration
        let settings = get_settings(&self.app_handle);

        // Handle InsanelyFastWhisper separately (subprocess call)
        {
            let engine_guard = self.lock_engine();
            if let Some(LoadedEngine::InsanelyFastWhisper) = engine_guard.as_ref() {
                drop(engine_guard);

                let ifw_model = settings
                    .insanely_fast_whisper_model
                    .as_deref()
                    .unwrap_or("openai/whisper-large-v3-turbo")
                    .to_string();

                let result = crate::insanely_fast_whisper_client::transcribe_audio(
                    &audio,
                    &ifw_model,
                    &settings.selected_language,
                )?;

                let corrected = if !settings.custom_words.is_empty() {
                    apply_custom_words(
                        &result,
                        &settings.custom_words,
                        settings.word_correction_threshold,
                    )
                } else {
                    result
                };
                let final_result = filter_transcription_output(&corrected);

                let et = std::time::Instant::now();
                info!(
                    "InsanelyFastWhisper transcription completed in {}ms",
                    (et - st).as_millis()
                );

                return Ok(final_result);
            }
        }

        // Perform transcription with the appropriate engine.
        // We use catch_unwind to prevent engine panics from poisoning the mutex,
        // which would make the app hang indefinitely on subsequent operations.
        let result = {
            let mut engine_guard = self.lock_engine();

            // Take the engine out so we own it during transcription.
            // If the engine panics, we simply don't put it back (effectively unloading it)
            // instead of poisoning the mutex.
            let mut engine = match engine_guard.take() {
                Some(e) => e,
                None => {
                    return Err(anyhow::anyhow!(
                        "Model failed to load after auto-load attempt. Please check your model settings."
                    ));
                }
            };

            // Release the lock before transcribing — no mutex held during the engine call
            drop(engine_guard);

            let transcribe_result = catch_unwind(AssertUnwindSafe(
                || -> Result<transcribe_rs::TranscriptionResult> {
                    match &mut engine {
                        LoadedEngine::Whisper(whisper_engine) => {
                            let whisper_language = if settings.selected_language == "auto" {
                                None
                            } else {
                                let normalized = if settings.selected_language == "zh-Hans"
                                    || settings.selected_language == "zh-Hant"
                                {
                                    "zh".to_string()
                                } else {
                                    settings.selected_language.clone()
                                };
                                Some(normalized)
                            };

                            let params = WhisperInferenceParams {
                                language: whisper_language,
                                translate: settings.translate_to_english,
                                ..Default::default()
                            };

                            whisper_engine
                                .transcribe_samples(audio, Some(params))
                                .map_err(|e| anyhow::anyhow!("Whisper transcription failed: {}", e))
                        }
                        LoadedEngine::Parakeet(parakeet_engine) => {
                            let params = ParakeetInferenceParams {
                                timestamp_granularity: TimestampGranularity::Segment,
                                ..Default::default()
                            };
                            parakeet_engine
                                .transcribe_samples(audio, Some(params))
                                .map_err(|e| {
                                    anyhow::anyhow!("Parakeet transcription failed: {}", e)
                                })
                        }
                        LoadedEngine::Moonshine(moonshine_engine) => moonshine_engine
                            .transcribe_samples(audio, None)
                            .map_err(|e| anyhow::anyhow!("Moonshine transcription failed: {}", e)),
                        LoadedEngine::MoonshineStreaming(streaming_engine) => streaming_engine
                            .transcribe_samples(audio, None)
                            .map_err(|e| {
                                anyhow::anyhow!("Moonshine streaming transcription failed: {}", e)
                            }),
                        LoadedEngine::SenseVoice(sense_voice_engine) => {
                            let language = match settings.selected_language.as_str() {
                                "zh" | "zh-Hans" | "zh-Hant" => SenseVoiceLanguage::Chinese,
                                "en" => SenseVoiceLanguage::English,
                                "ja" => SenseVoiceLanguage::Japanese,
                                "ko" => SenseVoiceLanguage::Korean,
                                "yue" => SenseVoiceLanguage::Cantonese,
                                _ => SenseVoiceLanguage::Auto,
                            };
                            let params = SenseVoiceInferenceParams {
                                language,
                                use_itn: true,
                            };
                            sense_voice_engine
                                .transcribe_samples(audio, Some(params))
                                .map_err(|e| {
                                    anyhow::anyhow!("SenseVoice transcription failed: {}", e)
                                })
                        }

                        LoadedEngine::InsanelyFastWhisper => {
                            unreachable!("InsanelyFastWhisper handled before catch_unwind")
                        }
                    }
                },
            ));

            match transcribe_result {
                Ok(inner_result) => {
                    // Success or normal error — put the engine back
                    let mut engine_guard = self.lock_engine();
                    *engine_guard = Some(engine);
                    inner_result?
                }
                Err(panic_payload) => {
                    // Engine panicked — do NOT put it back (it's in an unknown state).
                    // The engine is dropped here, effectively unloading it.
                    let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic".to_string()
                    };
                    error!(
                        "Transcription engine panicked: {}. Model has been unloaded.",
                        panic_msg
                    );

                    // Clear the model ID so it will be reloaded on next attempt
                    self.model_slot.set(MutationKind::Explicit, None);

                    let _ = self.app_handle.emit(
                        "model-state-changed",
                        ModelStateEvent {
                            event_type: "unloaded".to_string(),
                            model_id: None,
                            model_name: None,
                            error: Some(format!("Engine panicked: {}", panic_msg)),
                        },
                    );

                    return Err(anyhow::anyhow!(
                        "Transcription engine panicked: {}. The model has been unloaded and will reload on next attempt.",
                        panic_msg
                    ));
                }
            }
        };

        // Apply word correction if custom words are configured
        let corrected_result = if !settings.custom_words.is_empty() {
            apply_custom_words(
                &result.text,
                &settings.custom_words,
                settings.word_correction_threshold,
            )
        } else {
            result.text
        };

        // Filter out filler words and hallucinations
        let filtered_result = filter_transcription_output(&corrected_result);

        let et = std::time::Instant::now();
        let translation_note = if settings.translate_to_english {
            " (translated)"
        } else {
            ""
        };
        info!(
            "Transcription completed in {}ms{}",
            (et - st).as_millis(),
            translation_note
        );

        let final_result = filtered_result;

        if final_result.is_empty() {
            info!("Transcription result is empty");
        }

        Ok(final_result)
    }

    fn transcribe_target_sync(
        &self,
        model_id: &str,
        audio: Vec<f32>,
    ) -> Result<String, TranscriptionError> {
        self.wait_for_background_load();
        let _operation = self.lock_operation();
        let result = (|| {
            if self.get_current_model().as_deref() != Some(model_id) {
                self.load_model_inner(model_id, MutationKind::Explicit)
                    .map_err(|_| {
                        TranscriptionError::InvalidConfiguration(
                            "requested local model could not be loaded".into(),
                        )
                    })?;
            }
            if self.get_current_model().as_deref() != Some(model_id) || !self.is_model_loaded() {
                return Err(TranscriptionError::InvalidConfiguration(
                    "loaded local model does not match the requested target".into(),
                ));
            }
            self.transcribe_inner(audio)
                .map_err(|_| TranscriptionError::ProviderUnavailable)
        })();
        self.maybe_unload_immediately_inner("transcription");
        result
    }

    /// Like [`Self::transcribe_target_sync`] but the switch away from
    /// whatever was loaded before this call opens a [`RestoreTicket`]
    /// atomically with that switch (still holding the gate), so a caller
    /// that wants to restore the previous model afterward -- even if
    /// inference itself then failed -- can do so safely via
    /// [`Self::restore_ticket`]. The ticket is always returned, regardless
    /// of whether transcription succeeded: the caller is expected to always
    /// resolve it (see `TicketGuard` in `managers/transcription.rs`).
    pub fn transcribe_target_ticketed(
        &self,
        model_id: &str,
        audio: Vec<f32>,
    ) -> (Result<String, TranscriptionError>, RestoreTicket) {
        self.wait_for_background_load();
        let _operation = self.lock_operation();
        let baseline = self.get_current_model();
        let ticket = self.model_slot.open_ticket(baseline.clone());
        let result = (|| {
            if baseline.as_deref() != Some(model_id) {
                self.load_model_inner(model_id, MutationKind::Temporary)
                    .map_err(|_| {
                        TranscriptionError::InvalidConfiguration(
                            "requested local model could not be loaded".into(),
                        )
                    })?;
            }
            if self.get_current_model().as_deref() != Some(model_id) || !self.is_model_loaded() {
                return Err(TranscriptionError::InvalidConfiguration(
                    "loaded local model does not match the requested target".into(),
                ));
            }
            self.transcribe_inner(audio)
                .map_err(|_| TranscriptionError::ProviderUnavailable)
        })();
        self.maybe_unload_immediately_inner("transcription");
        (result, ticket)
    }
}

#[async_trait]
impl TranscriptionProvider for LocalProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        descriptor(&self.model_manager)
    }

    async fn transcribe(
        &self,
        model_id: &str,
        request: &TranscriptionRequest,
        _api_key: &str,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        let started = std::time::Instant::now();
        let text = self.transcribe_target_sync(model_id, request.audio_16khz_mono.clone())?;
        Ok(TranscriptionResult {
            text,
            provider_id: PROVIDER_ID.into(),
            model_id: model_id.to_string(),
            detected_language: None,
            latency: TranscriptionLatency {
                total_ms: started.elapsed().as_millis() as u64,
            },
        })
    }
}

#[cfg(test)]
mod operation_gate_tests {
    use super::LocalOperationGate;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn serializes_model_mutation_and_inference_operations() {
        let gate = Arc::new(LocalOperationGate::default());
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..8 {
            let gate = gate.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            workers.push(thread::spawn(move || {
                let _guard = gate.lock();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }

        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod versioned_model_slot_tests {
    //! Covers the restore-transaction races from issue #32: a reprocess or
    //! long-audio fallback must never restore a stale model over a newer
    //! explicit selection (`reprocess/set_active`, `fallback/set_active`),
    //! and two concurrent reprocesses must not strand each other's
    //! intermediate model (`reprocess/reprocess`) -- the earlier one's
    //! restore must still land on the *true original baseline* once the
    //! later one has unwound, not merely avoid crashing into it (the
    //! previous version of this test asserted the wrong thing: that the
    //! outer op's restore lands on the inner op's own temporary model,
    //! which is exactly the bug this ticket/stack mechanism fixes). Every
    //! test uses a `Barrier` or bounded `recv_timeout` to force the exact
    //! worst-case interleaving deterministically instead of hoping a
    //! `sleep` wins a race.
    use super::{MutationKind, RestoreOutcome, RestoreTicket, VersionedModelSlot};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    /// Simulates a genuine, permanent explicit selection (`set_active_model`,
    /// manual load/unload) -- bumps the explicit epoch, so any outstanding
    /// ticket opened before this call must be `Superseded`, never clobbered.
    fn explicit_switch(slot: &VersionedModelSlot, model_id: &str) {
        let _guard = slot.lock();
        slot.set(MutationKind::Explicit, Some(model_id.to_string()));
    }

    /// Simulates a temporary operation's (reprocess, fallback) own switch:
    /// atomically captures whatever is current as the ticket's restore
    /// baseline, opens the ticket, then switches to `model_id`.
    fn begin_temporary(slot: &VersionedModelSlot, model_id: &str) -> RestoreTicket {
        let _guard = slot.lock();
        let baseline = slot.current();
        let ticket = slot.open_ticket(baseline);
        slot.set(MutationKind::Temporary, Some(model_id.to_string()));
        ticket
    }

    /// Mirrors the real `apply` closure `LocalProviderState::restore_ticket`
    /// wires up: a load/unload that always succeeds.
    fn restore(slot: &VersionedModelSlot, ticket: RestoreTicket) -> RestoreOutcome {
        slot.restore_ticket(ticket, |target| {
            slot.set(MutationKind::Temporary, target.map(str::to_string));
            Ok(())
        })
    }

    /// Same, but the underlying load/unload always fails.
    fn restore_failing(slot: &VersionedModelSlot, ticket: RestoreTicket) -> RestoreOutcome {
        slot.restore_ticket(ticket, |_target| Err("load failed".to_string()))
    }

    /// reprocess/set_active: a `set_active_model` that completes after the
    /// reprocess's own switch, but before the reprocess gets to restore,
    /// must survive -- the reprocess's restore must see itself superseded
    /// rather than clobber it.
    #[test]
    fn reprocess_restore_does_not_clobber_a_concurrent_set_active_model() {
        let slot = Arc::new(VersionedModelSlot::new());
        explicit_switch(&slot, "M0");

        let ticket = begin_temporary(&slot, "X"); // reprocess's own switch

        let barrier = Arc::new(Barrier::new(2));
        let slot_for_set_active = slot.clone();
        let barrier_for_set_active = barrier.clone();
        let set_active = thread::spawn(move || {
            barrier_for_set_active.wait(); // reprocess has switched to X
            explicit_switch(&slot_for_set_active, "C");
            barrier_for_set_active.wait(); // set_active_model(C) has committed
        });

        barrier.wait();
        barrier.wait();
        set_active.join().unwrap();

        let outcome = slot.restore_ticket(ticket, |_target| {
            panic!("must not overwrite a newer explicit selection")
        });

        assert_eq!(outcome, RestoreOutcome::Superseded);
        assert_eq!(slot.current(), Some("C".to_string()));
    }

    /// fallback/set_active: identical shape to the reprocess case above, but
    /// modeling the long-audio fallback's deferred restore in `actions.rs`
    /// (same shared mechanism, same race).
    #[test]
    fn fallback_restore_does_not_clobber_a_concurrent_set_active_model() {
        let slot = Arc::new(VersionedModelSlot::new());
        explicit_switch(&slot, "small");

        let ticket = begin_temporary(&slot, "large");

        let barrier = Arc::new(Barrier::new(2));
        let slot_for_set_active = slot.clone();
        let barrier_for_set_active = barrier.clone();
        let set_active = thread::spawn(move || {
            barrier_for_set_active.wait();
            explicit_switch(&slot_for_set_active, "cloud-selected-model");
            barrier_for_set_active.wait();
        });

        barrier.wait();
        barrier.wait();
        set_active.join().unwrap();

        let outcome = slot.restore_ticket(ticket, |_target| {
            panic!("must not overwrite a newer explicit selection")
        });

        assert_eq!(outcome, RestoreOutcome::Superseded);
        assert_eq!(slot.current(), Some("cloud-selected-model".to_string()));
    }

    /// reprocess/reprocess: two concurrent reprocesses must not strand each
    /// other's intermediate model. A opens its ticket first (baseline M0,
    /// switch to X); B opens its ticket on top of that (baseline X, switch
    /// to Y) -- mirroring a second reprocess starting while the first is
    /// still in flight. A attempts its restore *before* B does; since A is
    /// not the top of the restore stack it must block rather than either
    /// clobbering Y or giving up, and once B has unwound back to X, A must
    /// then unwind all the way back to the *true* original baseline M0 --
    /// not stop at X, which was only ever A's own temporary model.
    #[test]
    fn two_concurrent_reprocesses_unwind_in_lifo_order_back_to_the_true_baseline() {
        let slot = Arc::new(VersionedModelSlot::new());
        explicit_switch(&slot, "M0");

        let ticket_a = begin_temporary(&slot, "X");
        let ticket_b = begin_temporary(&slot, "Y");
        assert_eq!(slot.current(), Some("Y".to_string()));

        let barrier = Arc::new(Barrier::new(2));
        let slot_for_a = slot.clone();
        let barrier_for_a = barrier.clone();
        let a = thread::spawn(move || {
            barrier_for_a.wait();
            restore(&slot_for_a, ticket_a)
        });

        barrier.wait();
        // Bias the schedule so A has a real chance to observe it is not the
        // top of the stack and start waiting before B restores. This only
        // affects which interleaving gets exercised -- correctness is
        // checked by the final assertions below regardless of ordering.
        thread::sleep(Duration::from_millis(50));
        let outcome_b = restore(&slot, ticket_b);
        let outcome_a = a.join().unwrap();

        assert_eq!(outcome_b, RestoreOutcome::Restored);
        assert_eq!(outcome_a, RestoreOutcome::Restored);
        assert_eq!(
            slot.current(),
            Some("M0".to_string()),
            "both temporary operations must fully unwind back to the true original baseline, \
             not strand A's own temporary model X"
        );
    }

    /// If a genuine explicit selection lands while an outer ticket is
    /// blocked waiting behind an inner one, the outer ticket must notice it
    /// was superseded once it finally gets its turn, instead of clobbering
    /// the newer selection just because "it was already waiting".
    #[test]
    fn a_waiting_restore_is_superseded_if_an_explicit_selection_lands_while_it_waits() {
        let slot = Arc::new(VersionedModelSlot::new());
        explicit_switch(&slot, "M0");
        let ticket_a = begin_temporary(&slot, "X");
        let ticket_b = begin_temporary(&slot, "Y");

        let (a_done_tx, a_done_rx) = std::sync::mpsc::channel();
        let slot_for_a = slot.clone();
        let a = thread::spawn(move || {
            let outcome = slot_for_a.restore_ticket(ticket_a, |target| {
                slot_for_a.set(MutationKind::Temporary, target.map(str::to_string));
                Ok(())
            });
            let _ = a_done_tx.send(());
            outcome
        });

        // A cannot be top yet (B is); give it time to actually start
        // blocking before the explicit selection lands.
        thread::sleep(Duration::from_millis(50));
        explicit_switch(&slot, "C"); // supersedes A while it waits behind B

        // Restoring B makes A the new top -- but A must notice the
        // explicit epoch moved on in the meantime and refuse to restore.
        let outcome_b = restore(&slot, ticket_b);
        a_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("A's restore must complete (as Superseded), not hang");
        let outcome_a = a.join().unwrap();

        assert_eq!(outcome_b, RestoreOutcome::Superseded);
        assert_eq!(outcome_a, RestoreOutcome::Superseded);
        assert_eq!(slot.current(), Some("C".to_string()));
    }

    #[test]
    fn restore_is_a_no_op_when_nothing_changed() {
        let slot = VersionedModelSlot::new();
        explicit_switch(&slot, "M0");
        let ticket = begin_temporary(&slot, "M0"); // "switches" to the same model
        let outcome = slot.restore_ticket(ticket, |_| {
            panic!("must not attempt a load when already matching")
        });
        assert_eq!(outcome, RestoreOutcome::Restored);
    }

    #[test]
    fn restore_failure_is_reported_not_silently_dropped() {
        let slot = VersionedModelSlot::new();
        explicit_switch(&slot, "M0");
        let ticket = begin_temporary(&slot, "X");
        let outcome = restore_failing(&slot, ticket);
        assert_eq!(outcome, RestoreOutcome::Failed("load failed".to_string()));
        // The failed restore must not have silently pretended to succeed.
        assert_eq!(slot.current(), Some("X".to_string()));
    }

    /// A ticket whose own switch never actually happened (e.g. the
    /// requested model failed to load) must be abandoned rather than left
    /// stranded at the top of the restore stack, or it would permanently
    /// block every ticket beneath it.
    #[test]
    fn abandoning_a_ticket_lets_the_one_beneath_it_proceed() {
        let slot = VersionedModelSlot::new();
        explicit_switch(&slot, "M0");
        let ticket_a = begin_temporary(&slot, "X");
        let ticket_b = {
            let _guard = slot.lock();
            let baseline = slot.current();
            slot.open_ticket(baseline)
        };
        slot.abandon_ticket(&ticket_b);

        let outcome = restore(&slot, ticket_a);
        assert_eq!(outcome, RestoreOutcome::Restored);
        assert_eq!(slot.current(), Some("M0".to_string()));
    }
}

#[cfg(test)]
mod watcher_shutdown_tests {
    //! Covers the watcher deadlock/self-join bug from issue #32: the old
    //! `if let Some(handle) = watcher_handle.lock().unwrap().take() { handle.join() }`
    //! pattern held the mutex guard for the whole `if let` body (a classic
    //! Rust temporary-lifetime-extension footgun), including across the
    //! blocking `.join()` call. Combined with `Drop` running per-clone
    //! instead of once, a value living inside the watcher thread trying to
    //! reach the same mutex during its own teardown would deadlock against
    //! the joiner still holding it.
    use super::WatcherHandle;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn join_releases_the_guard_before_blocking_so_the_watched_thread_can_finish() {
        // Mirrors a value living inside the watcher thread (e.g. a
        // `LocalProvider` clone) whose own teardown reaches for the same
        // mutex the joiner uses -- it cannot finish until that mutex is
        // free. `go_probe` lets the test control exactly when this thread
        // attempts to acquire `watcher.0`, so the schedule below can
        // deterministically put the joiner into its critical section first.
        let (go_probe_tx, go_probe_rx) = std::sync::mpsc::channel::<()>();
        let watcher_cell: Arc<Mutex<Option<Arc<WatcherHandle>>>> = Arc::new(Mutex::new(None));
        let watcher_cell_for_thread = watcher_cell.clone();

        let handle = thread::spawn(move || {
            go_probe_rx.recv().unwrap();
            let watcher = watcher_cell_for_thread.lock().unwrap().clone().unwrap();
            watcher.probe_lock();
        });

        let watcher = Arc::new(WatcherHandle::empty());
        watcher.install(handle);
        *watcher_cell.lock().unwrap() = Some(watcher.clone());

        let (tx, rx) = std::sync::mpsc::channel();
        let joiner_watcher = watcher.clone();
        thread::spawn(move || {
            joiner_watcher.join();
            let _ = tx.send(());
        });

        // Bias the schedule so the joiner thread has already entered
        // `join()` -- and, under the old buggy `if let Some(handle) =
        // watcher_handle.lock().unwrap().take() { handle.join() }` pattern,
        // is already holding the guard for the whole `if let` body -- before
        // the watched thread is allowed to attempt `probe_lock()`. This only
        // biases which interleaving gets exercised; the actual pass/fail
        // check below is the deterministic part (a hard timeout, not a
        // sleep). Verified against the old pattern: it reliably times out.
        thread::sleep(Duration::from_millis(100));
        go_probe_tx.send(()).unwrap();

        rx.recv_timeout(Duration::from_secs(2))
            .expect("join() must release the mutex before blocking, or this deadlocks");
    }

    #[test]
    fn join_is_idempotent_across_concurrent_callers() {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let ready_clone = ready.clone();
        let handle = thread::spawn(move || {
            ready_clone.wait();
            thread::sleep(Duration::from_millis(20));
        });
        let watcher = Arc::new(WatcherHandle::empty());
        watcher.install(handle);
        ready.wait();

        // Simulates every clone's `Drop` racing to run the same
        // shutdown/join logic; only one may actually join, the rest must be
        // harmless no-ops, and all must terminate promptly.
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..3 {
            let watcher = watcher.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                watcher.join();
                let _ = tx.send(());
            });
        }
        drop(tx);

        for _ in 0..3 {
            rx.recv_timeout(Duration::from_secs(2))
                .expect("concurrent join() callers must all terminate promptly");
        }
    }

    /// Reproduces the scenario the fail-closed review flagged: the watcher
    /// thread's `Weak::upgrade()` can make it the *last* strong owner of
    /// `LocalProviderState` for the rest of that loop iteration. If every
    /// other owner is dropped during that window, `WatcherHandle::drop` (and
    /// therefore `join()`) runs *on the watcher thread itself* -- joining its
    /// own `JoinHandle` would deadlock forever, since the thread cannot
    /// finish executing this very call until it returns. `join()` must
    /// detect that and detach instead of blocking.
    #[test]
    fn join_detects_self_join_and_detaches_instead_of_deadlocking() {
        let watcher = Arc::new(WatcherHandle::empty());
        let watcher_for_thread = watcher.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let handle = thread::spawn(move || {
            ready_rx.recv().unwrap();
            // Call join() on our own WatcherHandle from within the very
            // thread it wraps -- mirrors `LocalProviderState::drop` running
            // on the watcher thread because it ended up holding the last
            // strong `Arc`.
            watcher_for_thread.join();
            let _ = done_tx.send(());
        });
        watcher.install(handle);
        ready_tx.send(()).unwrap();

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("join() must detect a self-join and detach instead of deadlocking");
    }
}

// `LocalProviderState` needs no `Drop` impl of its own: `watcher_handle`
// (a `WatcherHandle`) already joins the idle-watcher thread when it is
// dropped as a field, and that only happens once -- when the very last
// `LocalProvider` (`Arc<LocalProviderState>`) handle goes away, never once
// per cheap clone. The idle watcher itself holds only a `Weak` reference
// (see `LocalProvider::new`), so it can never be the thing keeping that last
// reference alive.
