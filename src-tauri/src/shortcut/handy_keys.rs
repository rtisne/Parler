//! Handy-keys based keyboard shortcut implementation
//!
//! This module provides an alternative to Tauri's global-shortcut plugin
//! using the handy-keys library for more control over keyboard events.
//!
//! ## Architecture
//!
//! The implementation uses a dedicated manager thread that owns the `HotkeyManager`:
//!
//! ```text
//! ┌─────────────────┐     commands      ┌──────────────────────┐
//! │   Main Thread   │ ───────────────▶ │   Manager Thread     │
//! │                 │   (via channel)   │                      │
//! │ - register()    │                   │ - owns HotkeyManager │
//! │ - unregister()  │                   │ - polls for events   │
//! └─────────────────┘                   │ - dispatches actions │
//!                                       └──────────────────────┘
//! ```
//!
//! This design ensures thread-safety since `HotkeyManager` is only accessed
//! from a single thread. Commands (register/unregister) are sent via an mpsc
//! channel and responses are synchronously awaited.
//!
//! ## OS-level key blocking and the Linux compromise
//!
//! Only push-to-talk transcribe triggers should be *blocked* system-wide (see
//! [`should_block_os_key`]): holding e.g. `option+space` must not type spaces
//! into the focused app. Every other binding must stay passive so it never
//! swallows a keystroke destined for another application.
//!
//! On **Windows/macOS** this is implemented with two managers: a blocking
//! manager (a consuming event tap) for the transcribe triggers and a passive,
//! non-blocking manager for everything else.
//!
//! On **Linux** we deliberately run only a **single passive manager**. Each
//! `HotkeyManager` spawns a `KeyboardListener` that, on Linux, performs an
//! *exclusive* `rdev::grab()` evdev grab of the input devices. Two grabs are
//! mutually exclusive: a second listener cannot obtain the grab and would spin
//! retrying, leaving one of the two managers permanently deaf depending on
//! thread scheduling. To keep every binding working we route all Linux bindings
//! — including the transcribe triggers — through the passive manager. The
//! trade-off is explicit: **on Linux the transcribe keys are NOT OS-blocked**
//! and will still reach the focused application. The policy classification in
//! [`should_block_os_key`] is preserved and still used on Windows/macOS.
//!
//! ## Collision handling
//!
//! A single canonical [`HotkeyRegistry`] maps each physical combo to the
//! binding that owns it, shared across both manager categories. Registration
//! rejects inter-manager collisions atomically (in either registration order)
//! *before* touching an OS manager, so the same physical combo can never be
//! claimed by two bindings even when they route to different managers.
//!
//! ## Recording Mode
//!
//! For UI key capture, a separate `KeyboardListener` is created on-demand and
//! polled from a dedicated recording thread. The whole capture session lives
//! behind a single [`RecordingState`] whose entire lifecycle — flag, listener,
//! timestamp and per-session stop signal — is protected by *one* mutex, so its
//! transitions are linearizable: a `claim` (no concurrent double-start), the
//! `install_session` handoff, an idempotent `teardown`, and an autonomous
//! stale-session expiry (see [`MAX_RECORDING_DURATION`]) driven by the recording
//! loop itself so it fires even with no keyboard events.

use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener};
use log::{debug, error, info};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{self, get_settings, ShortcutBinding};
use crate::transcription_coordinator::is_transcribe_binding;

use super::handler::handle_shortcut_event;

/// Whether a binding must go through the OS-level *blocking* manager.
///
/// Only push-to-talk transcribe triggers are suppressed system-wide: holding
/// e.g. `option+space` must not type spaces into the focused app, so the
/// matched combo has to be consumed by a blocking event tap. Every other
/// binding (pause, cancel/escape, history, copy-latest, post-processing action
/// digits, …) is passive and must never swallow a keystroke for other apps.
///
/// This is the *policy* classification. It is honoured on Windows/macOS; on
/// Linux only the passive manager exists (see module docs) so the physical
/// routing in [`physical_category`] forces every binding passive there.
fn should_block_os_key(binding_id: &str) -> bool {
    is_transcribe_binding(binding_id)
}

/// Which physical OS manager a binding is registered with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ManagerCategory {
    /// Consuming event tap; the combo is blocked from other apps.
    Blocking,
    /// Non-blocking listener; the combo still reaches other apps.
    Passive,
}

/// The physical manager a binding is routed to.
///
/// On Linux this is always [`ManagerCategory::Passive`]: two concurrent
/// `rdev::grab()` listeners are mutually exclusive, so we run a single passive
/// manager and the transcribe triggers are intentionally NOT OS-blocked. On
/// Windows/macOS the policy classification from [`should_block_os_key`] decides.
fn physical_category(binding_id: &str) -> ManagerCategory {
    #[cfg(target_os = "linux")]
    {
        // The policy classification is preserved for reference, but Linux runs a
        // single passive manager (two rdev grabs are mutually exclusive), so
        // every binding — including transcribe triggers — is passive here and is
        // therefore NOT OS-blocked (see module docs).
        let _would_block_on_desktop = should_block_os_key(binding_id);
        ManagerCategory::Passive
    }
    #[cfg(not(target_os = "linux"))]
    {
        if should_block_os_key(binding_id) {
            ManagerCategory::Blocking
        } else {
            ManagerCategory::Passive
        }
    }
}

/// A committed registration entry, keyed by binding id.
struct Registration<Id> {
    hotkey: Hotkey,
    id: Id,
    category: ManagerCategory,
}

/// Canonical, single source of truth for every registered physical hotkey.
///
/// Shared across manager categories so a given physical combo can be owned by at
/// most one binding regardless of which manager it routes to. Generic over the
/// OS id type so the collision/cleanup logic can be unit-tested without a real
/// [`HotkeyManager`] (whose [`HotkeyId`] cannot be constructed outside the
/// crate).
struct HotkeyRegistry<Id = HotkeyId> {
    /// Physical combo -> owning binding id (global collision map).
    by_hotkey: HashMap<Hotkey, String>,
    /// Binding id -> registration details (for unregister/routing).
    by_binding: HashMap<String, Registration<Id>>,
    /// Per-category dispatch maps. `HotkeyId`s are allocated per-manager and
    /// would otherwise collide between managers, so each category keeps its own.
    blocking_ids: HashMap<Id, (String, String)>,
    passive_ids: HashMap<Id, (String, String)>,
}

impl<Id: Copy + Eq + Hash> Default for HotkeyRegistry<Id> {
    fn default() -> Self {
        Self {
            by_hotkey: HashMap::new(),
            by_binding: HashMap::new(),
            blocking_ids: HashMap::new(),
            passive_ids: HashMap::new(),
        }
    }
}

impl<Id: Copy + Eq + Hash> HotkeyRegistry<Id> {
    /// Read-only inter-manager collision check against the canonical map.
    ///
    /// Rejects a physical combo already owned by a *different* binding, in
    /// either registration order (blocking↔passive). Re-checking the same
    /// binding's own combo is not a collision.
    fn check_collision(&self, hotkey: &Hotkey, binding_id: &str) -> Result<(), String> {
        if let Some(existing) = self.by_hotkey.get(hotkey) {
            if existing != binding_id {
                return Err(format!(
                    "Hotkey combination already bound to '{}'",
                    existing
                ));
            }
        }
        Ok(())
    }

    /// Commit a successful OS registration into every map.
    ///
    /// Infallible and only ever called *after* the underlying manager accepted
    /// the hotkey and after [`check_collision`](Self::check_collision) passed,
    /// so a failed OS registration never pollutes the registry.
    fn commit(
        &mut self,
        binding_id: &str,
        hotkey: Hotkey,
        hotkey_string: &str,
        id: Id,
        category: ManagerCategory,
    ) {
        self.by_hotkey.insert(hotkey, binding_id.to_string());
        self.by_binding.insert(
            binding_id.to_string(),
            Registration {
                hotkey,
                id,
                category,
            },
        );
        let ids = match category {
            ManagerCategory::Blocking => &mut self.blocking_ids,
            ManagerCategory::Passive => &mut self.passive_ids,
        };
        ids.insert(id, (binding_id.to_string(), hotkey_string.to_string()));
    }

    /// Remove a binding from every map. Returns the `(id, category)` so the
    /// caller can unregister it from the correct OS manager.
    fn remove(&mut self, binding_id: &str) -> Option<(Id, ManagerCategory)> {
        let reg = self.by_binding.remove(binding_id)?;
        self.by_hotkey.remove(&reg.hotkey);
        let ids = match reg.category {
            ManagerCategory::Blocking => &mut self.blocking_ids,
            ManagerCategory::Passive => &mut self.passive_ids,
        };
        ids.remove(&reg.id);
        Some((reg.id, reg.category))
    }

    /// Resolve an incoming OS event `id` (within its manager category) back to
    /// the owning `(binding_id, hotkey_string)`.
    fn dispatch_target(&self, category: ManagerCategory, id: &Id) -> Option<&(String, String)> {
        match category {
            ManagerCategory::Blocking => self.blocking_ids.get(id),
            ManagerCategory::Passive => self.passive_ids.get(id),
        }
    }

    /// Peek a binding's OS handle without mutating the registry.
    ///
    /// Lets a caller attempt the OS unregister *first* and only
    /// [`remove`](Self::remove) the registry state once it succeeds (see
    /// [`unregister_transactional`](Self::unregister_transactional)).
    fn os_handle(&self, binding_id: &str) -> Option<(Id, ManagerCategory)> {
        self.by_binding
            .get(binding_id)
            .map(|reg| (reg.id, reg.category))
    }

    /// Transactionally register (or replace) a binding.
    ///
    /// Ordering guarantees the previously valid registration for `binding_id`
    /// survives any rejection:
    ///
    /// 1. re-registering the *exact* same combo for this binding is a no-op;
    /// 2. an inter-manager collision is rejected before any OS or registry
    ///    mutation (stage/check), so the old registration is untouched;
    /// 3. the **new** combo is registered with the OS *first*, while any prior
    ///    (distinct) combo for this binding stays live — if the OS rejects the
    ///    new combo the old registry entry and OS hook are still intact;
    /// 4. only once the new combo is live do we drop the prior registration and
    ///    commit the new one (commit). The OS release of the old combo is
    ///    best-effort: its id is already off the dispatch map, so a failure only
    ///    leaks an inert OS hook and is logged rather than lost.
    fn register_transactional(
        &mut self,
        binding_id: &str,
        hotkey: Hotkey,
        hotkey_string: &str,
        category: ManagerCategory,
        os_register: impl FnOnce(Hotkey) -> Result<Id, String>,
        os_unregister: impl FnOnce(Id, ManagerCategory) -> Result<(), String>,
    ) -> Result<(), String> {
        // (1) Re-registering the exact same combo for this binding is a no-op.
        if let Some(reg) = self.by_binding.get(binding_id) {
            if reg.hotkey == hotkey {
                return Ok(());
            }
        }

        // (2) Reject a combo owned by a *different* binding before touching the OS
        // or the registry, so a rejected registration changes nothing.
        self.check_collision(&hotkey, binding_id)?;

        // (3) Register the new combo first; a failure here leaves the old
        // registration (registry entry + OS hook) fully intact.
        let new_id = os_register(hotkey)?;

        // (4) Commit: drop any prior registration for this binding, releasing its
        // OS hook best-effort, then record the new one.
        if let Some((old_id, old_category)) = self.remove(binding_id) {
            if let Err(e) = os_unregister(old_id, old_category) {
                error!(
                    "Failed to release previous hotkey for '{}' after replacement: {}",
                    binding_id, e
                );
            }
        }

        self.commit(binding_id, hotkey, hotkey_string, new_id, category);
        Ok(())
    }

    /// Transactionally unregister a binding.
    ///
    /// The OS unregister runs *first*; the canonical registry state is dropped
    /// only after it succeeds, so a failed OS unregister never leaves the
    /// registry claiming a still-registered combo is free.
    fn unregister_transactional(
        &mut self,
        binding_id: &str,
        os_unregister: impl FnOnce(Id, ManagerCategory) -> Result<(), String>,
    ) -> Result<(), String> {
        let Some((id, category)) = self.os_handle(binding_id) else {
            return Ok(());
        };
        os_unregister(id, category)?;
        self.remove(binding_id);
        Ok(())
    }
}

/// Commands that can be sent to the hotkey manager thread
enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
    },
    Shutdown,
}

/// Maximum duration a binding-recording session may suppress global shortcuts.
///
/// Safety net: if the frontend never calls `stop_recording` (e.g. the webview
/// crashes mid-recording), suppression auto-expires after this window instead
/// of leaving every global shortcut disabled until the app restarts. Recording
/// a shortcut takes a second or two, so this is far longer than any real use.
const MAX_RECORDING_DURATION: Duration = Duration::from_secs(30);

/// A single in-flight key-recording session for the settings UI.
struct RecordingSession {
    /// OS key listener feeding the recording loop. `None` only in unit tests,
    /// which exercise the session lifecycle without a real OS hook.
    listener: Option<KeyboardListener>,
    /// The binding currently being recorded. Cleared on teardown/expiry.
    #[allow(dead_code)]
    binding_id: String,
    /// When recording started; drives auto-expiry.
    started_at: Instant,
    /// Per-session stop flag for the recording loop. Cleared on teardown so the
    /// exact loop bound to *this* session exits and no stale listener survives.
    /// Also the identity token: a loop only touches the session whose `running`
    /// flag it owns (see [`RecordingState::poll_for_loop`]).
    running: Arc<AtomicBool>,
}

/// Lifecycle phase of the settings-UI key capture, all transitions serialized by
/// [`RecordingState`]'s single mutex.
enum RecordingPhase {
    /// No capture in progress.
    Idle,
    /// A slot has been claimed but the live session (its OS listener) is still
    /// being set up. Carries the claim's generation so a teardown or a newer
    /// claim during setup is detected by [`install_session`] and the stale
    /// listener is dropped instead of installed.
    Claimed(u64),
    /// A live capture session.
    Active(RecordingSession),
}

/// Single source of truth for the settings-UI key capture session.
///
/// The *entire* lifecycle lives behind one mutex, so the flag, listener, timing
/// and per-session stop signal can never disagree: `claim → install_session →
/// teardown/expiry` is a linearizable sequence of locked transitions rather than
/// a split atomic-flag-plus-separate-mutex that races (a teardown clearing a
/// session a concurrent claim just installed, or a claim installing a listener a
/// concurrent teardown already cancelled). A monotonic `generation` disambiguates
/// the claim → install handoff.
struct RecordingState {
    inner: Mutex<RecordingInner>,
}

struct RecordingInner {
    phase: RecordingPhase,
    /// Monotonic claim counter; each [`claim`](RecordingState::claim) takes the
    /// next value so an install/abort can prove it still owns its claim.
    next_generation: u64,
}

/// One step for the recording loop, produced under the lock by
/// [`RecordingState::poll_for_loop`].
enum RecordingStep {
    /// A key event for the frontend.
    Event(handy_keys::KeyEvent),
    /// Session still live, no event pending — the loop should idle briefly.
    Idle,
    /// The loop's session is gone (torn down, expired, or replaced); stop.
    Stop,
}

impl RecordingState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(RecordingInner {
                phase: RecordingPhase::Idle,
                next_generation: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecordingInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Expire a live session that has outlived [`MAX_RECORDING_DURATION`],
    /// returning whether it did. Runs under an already-held lock.
    fn expire_locked(inner: &mut RecordingInner, now: Instant) -> bool {
        if let RecordingPhase::Active(session) = &inner.phase {
            if now.saturating_duration_since(session.started_at) >= MAX_RECORDING_DURATION {
                session.running.store(false, Ordering::SeqCst);
                inner.phase = RecordingPhase::Idle;
                return true;
            }
        }
        false
    }

    /// Atomically claim the recording slot, returning a generation token for the
    /// subsequent [`install_session`](Self::install_session).
    ///
    /// A stale (expired) session is torn down first so a crashed recording can't
    /// block new ones. Returns `Err` if a claim or live session already exists.
    /// `now` is injected so expiry is testable without sleeping.
    fn claim(&self, now: Instant) -> Result<u64, String> {
        let mut inner = self.lock();
        Self::expire_locked(&mut inner, now);
        match inner.phase {
            RecordingPhase::Idle => {
                let generation = inner.next_generation;
                inner.next_generation = inner.next_generation.wrapping_add(1);
                inner.phase = RecordingPhase::Claimed(generation);
                Ok(generation)
            }
            _ => Err("Already recording".to_string()),
        }
    }

    /// Release a claim taken by [`claim`](Self::claim) when session setup fails
    /// before a session is installed. No-op if the claim was already superseded.
    fn abort_claim(&self, generation: u64) {
        let mut inner = self.lock();
        if matches!(inner.phase, RecordingPhase::Claimed(g) if g == generation) {
            inner.phase = RecordingPhase::Idle;
        }
    }

    /// Install the live session for a prior claim and return its per-session stop
    /// flag for the recording loop.
    ///
    /// Returns `None` if the claim was already torn down (teardown/expiry) or
    /// superseded by a newer claim during listener setup; the caller must then
    /// drop the listener and not spawn a loop. `now` is injected for tests.
    fn install_session(
        &self,
        generation: u64,
        binding_id: String,
        listener: Option<KeyboardListener>,
        now: Instant,
    ) -> Option<Arc<AtomicBool>> {
        let mut inner = self.lock();
        match inner.phase {
            RecordingPhase::Claimed(g) if g == generation => {
                let running = Arc::new(AtomicBool::new(true));
                inner.phase = RecordingPhase::Active(RecordingSession {
                    listener,
                    binding_id,
                    started_at: now,
                    running: Arc::clone(&running),
                });
                Some(running)
            }
            _ => None,
        }
    }

    /// Fully and idempotently tear down any session or claim: stop the loop,
    /// drop the listener, and clear the flag, binding id, and timestamp.
    fn teardown(&self) {
        let mut inner = self.lock();
        if let RecordingPhase::Active(session) = &inner.phase {
            session.running.store(false, Ordering::SeqCst);
        }
        inner.phase = RecordingPhase::Idle;
    }

    /// One watchdog + poll step for a recording loop bound to `running`.
    ///
    /// Runs the stale-session watchdog first, so an abandoned session expires
    /// autonomously from the loop's own ticking even when no keyboard events ever
    /// arrive. Then, only if this loop still owns the live session (identified by
    /// its `running` flag — a session installed *after* a teardown belongs to a
    /// different loop), returns the next key event if any. Returns
    /// [`RecordingStep::Stop`] once the loop must exit.
    fn poll_for_loop(&self, running: &Arc<AtomicBool>, now: Instant) -> RecordingStep {
        let mut inner = self.lock();
        Self::expire_locked(&mut inner, now);
        match &inner.phase {
            RecordingPhase::Active(session) if Arc::ptr_eq(&session.running, running) => {
                match session.listener.as_ref().and_then(|l| l.try_recv()) {
                    Some(event) => RecordingStep::Event(event),
                    None => RecordingStep::Idle,
                }
            }
            _ => RecordingStep::Stop,
        }
    }

    /// Whether the UI is actively capturing keys for a new binding.
    ///
    /// Returns false (and fully tears the session down) once the session has
    /// outlived [`MAX_RECORDING_DURATION`], so a frontend that never calls
    /// `stop_recording` can't leave global shortcuts suppressed or a stale
    /// listener alive. A claim mid-setup still counts as capturing.
    fn is_capturing(&self) -> bool {
        self.is_capturing_at(Instant::now())
    }

    fn is_capturing_at(&self, now: Instant) -> bool {
        let mut inner = self.lock();
        Self::expire_locked(&mut inner, now);
        !matches!(inner.phase, RecordingPhase::Idle)
    }
}

/// State for the handy-keys shortcut manager
pub struct HandyKeysState {
    /// Channel to send commands to the manager thread (wrapped in Mutex for Sync)
    command_sender: Mutex<Sender<ManagerCommand>>,
    /// Handle to the manager thread (wrapped in Mutex for Sync, allows proper join on drop)
    thread_handle: Mutex<Option<JoinHandle<()>>>,
    /// Settings-UI key capture session (see [`RecordingState`]).
    recording: RecordingState,
}

/// Key event sent to frontend during recording mode
#[derive(Debug, Clone, Serialize, Type)]
pub struct FrontendKeyEvent {
    /// Currently pressed modifier keys
    pub modifiers: Vec<String>,
    /// The key that was pressed (if any)
    pub key: Option<String>,
    /// Whether this is a key down event
    pub is_key_down: bool,
    /// The full hotkey string (e.g., "option+space")
    pub hotkey_string: String,
}

impl HandyKeysState {
    /// Create a new HandyKeysState
    pub fn new(app: AppHandle) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ManagerCommand>();

        // Start the manager thread
        let app_clone = app.clone();
        let thread_handle = thread::spawn(move || {
            Self::manager_thread(cmd_rx, app_clone);
        });

        Ok(Self {
            command_sender: Mutex::new(cmd_tx),
            thread_handle: Mutex::new(Some(thread_handle)),
            recording: RecordingState::new(),
        })
    }

    /// The main manager thread - owns the HotkeyManager(s) and processes commands
    fn manager_thread(cmd_rx: Receiver<ManagerCommand>, app: AppHandle) {
        info!("handy-keys manager thread started");

        // The passive, non-blocking manager exists on every platform and handles
        // every binding that must NOT swallow keys for other apps.
        let passive_manager = match HotkeyManager::new() {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to create passive HotkeyManager: {}", e);
                return;
            }
        };

        // The blocking manager (a consuming event tap) exists only on
        // Windows/macOS. On Linux a second listener would fight the passive
        // manager for an exclusive `rdev::grab()` and lose, so we keep a single
        // passive listener there and accept that transcribe keys are not
        // OS-blocked on Linux (see module docs).
        #[cfg(not(target_os = "linux"))]
        let blocking_manager = match HotkeyManager::new_with_blocking() {
            Ok(m) => Some(m),
            Err(e) => {
                error!("Failed to create blocking HotkeyManager: {}", e);
                return;
            }
        };
        #[cfg(target_os = "linux")]
        let blocking_manager: Option<HotkeyManager> = None;

        let mut registry: HotkeyRegistry = HotkeyRegistry::default();

        loop {
            // Drain hotkey events from every active manager (non-blocking),
            // dispatching each via that manager's own id map.
            for (manager, category) in [
                (blocking_manager.as_ref(), ManagerCategory::Blocking),
                (Some(&passive_manager), ManagerCategory::Passive),
            ] {
                let Some(manager) = manager else {
                    continue;
                };
                while let Some(event) = manager.try_recv() {
                    if let Some((binding_id, hotkey_string)) =
                        registry.dispatch_target(category, &event.id)
                    {
                        // While the user is recording a new binding in the settings
                        // UI, suppress all global shortcut actions. Otherwise a
                        // registered shortcut (e.g. a modifier-only "Left Ctrl"
                        // transcribe binding) fires the moment its keys are pressed
                        // during recording, triggering transcription and cutting the
                        // capture short. Events are still drained so they don't queue
                        // up and fire once recording ends.
                        if app
                            .try_state::<HandyKeysState>()
                            .is_some_and(|state| state.recording.is_capturing())
                        {
                            continue;
                        }
                        debug!(
                            "handy-keys event: binding={}, hotkey={}, state={:?}",
                            binding_id, hotkey_string, event.state
                        );
                        let is_pressed = event.state == HotkeyState::Pressed;
                        handle_shortcut_event(&app, binding_id, hotkey_string, is_pressed);
                    }
                }
            }

            // Check for commands (non-blocking with timeout)
            match cmd_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(cmd) => match cmd {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                    } => {
                        let result = Self::do_register(
                            &mut registry,
                            blocking_manager.as_ref(),
                            &passive_manager,
                            &binding_id,
                            &hotkey_string,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                    } => {
                        let result = Self::do_unregister(
                            &mut registry,
                            blocking_manager.as_ref(),
                            &passive_manager,
                            &binding_id,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Shutdown => {
                        info!("handy-keys manager thread shutting down");
                        break;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No command, continue
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Command channel disconnected, shutting down");
                    break;
                }
            }
        }

        info!("handy-keys manager thread stopped");
    }

    /// Register a hotkey.
    ///
    /// Rejects inter-manager physical collisions *before* touching an OS manager
    /// and only records the registration once the OS manager accepts it, so a
    /// failed OS registration never pollutes the canonical registry.
    fn do_register(
        registry: &mut HotkeyRegistry,
        blocking_manager: Option<&HotkeyManager>,
        passive_manager: &HotkeyManager,
        binding_id: &str,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let hotkey: Hotkey = hotkey_string
            .parse()
            .map_err(|e| format!("Failed to parse hotkey '{}': {}", hotkey_string, e))?;

        let category = physical_category(binding_id);

        // Stage/check + commit-or-preserve is handled inside the registry: a
        // collision or a failed OS registration leaves any prior registration for
        // this binding intact (see [`HotkeyRegistry::register_transactional`]).
        registry.register_transactional(
            binding_id,
            hotkey,
            hotkey_string,
            category,
            |hk| {
                let manager = match category {
                    ManagerCategory::Blocking => blocking_manager.unwrap_or(passive_manager),
                    ManagerCategory::Passive => passive_manager,
                };
                manager
                    .register(hk)
                    .map_err(|e| format!("Failed to register hotkey: {}", e))
            },
            |id, cat| {
                let manager = match cat {
                    ManagerCategory::Blocking => blocking_manager.unwrap_or(passive_manager),
                    ManagerCategory::Passive => passive_manager,
                };
                manager
                    .unregister(id)
                    .map_err(|e| format!("Failed to unregister hotkey: {}", e))
            },
        )?;

        debug!(
            "Registered handy-keys shortcut: {} -> {:?} ({:?})",
            binding_id, hotkey, category
        );
        Ok(())
    }

    /// Unregister a hotkey. The OS unregister runs first; the canonical registry
    /// state is dropped only once it succeeds, so a failed OS unregister never
    /// leaves the registry claiming a still-registered combo is free.
    fn do_unregister(
        registry: &mut HotkeyRegistry,
        blocking_manager: Option<&HotkeyManager>,
        passive_manager: &HotkeyManager,
        binding_id: &str,
    ) -> Result<(), String> {
        registry.unregister_transactional(binding_id, |id, category| {
            let manager = match category {
                ManagerCategory::Blocking => blocking_manager.unwrap_or(passive_manager),
                ManagerCategory::Passive => passive_manager,
            };
            manager
                .unregister(id)
                .map_err(|e| format!("Failed to unregister hotkey: {}", e))
        })?;
        debug!("Unregistered handy-keys shortcut: {}", binding_id);
        Ok(())
    }

    /// Register a shortcut binding
    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send register command")?;

        rx.recv()
            .map_err(|_| "Failed to receive register response")?
    }

    /// Unregister a shortcut binding
    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send unregister command")?;

        rx.recv()
            .map_err(|_| "Failed to receive unregister response")?
    }

    /// Start recording mode for a specific binding.
    ///
    /// The [`RecordingState::claim`] is atomic, so a concurrent second call is
    /// rejected instead of replacing the live listener.
    pub fn start_recording(&self, app: &AppHandle, binding_id: String) -> Result<(), String> {
        // Atomic claim: rejects a concurrent double-start and expires a stale one.
        let generation = self.recording.claim(Instant::now())?;

        // Create a new keyboard listener for recording. If this fails we must
        // release the claim so recording isn't wedged "on" forever.
        let listener = match KeyboardListener::new() {
            Ok(l) => l,
            Err(e) => {
                self.recording.abort_claim(generation);
                return Err(format!("Failed to create keyboard listener: {}", e));
            }
        };

        // Install the session. `None` means a concurrent teardown (stop_recording
        // or expiry) cancelled this claim during listener setup: the listener is
        // dropped here and no loop is spawned, leaving the state coherently idle.
        let running = match self.recording.install_session(
            generation,
            binding_id,
            Some(listener),
            Instant::now(),
        ) {
            Some(running) => running,
            None => {
                debug!("handy-keys recording claim was cancelled during setup");
                return Ok(());
            }
        };

        // Start a thread to emit key events to the frontend
        let app_clone = app.clone();
        thread::spawn(move || {
            Self::recording_loop(app_clone, running);
        });

        debug!("Started handy-keys recording mode");
        Ok(())
    }

    /// Recording loop - emits key events to frontend during recording.
    ///
    /// Doubles as the session watchdog: every tick runs the stale-session expiry
    /// under the lock (via [`RecordingState::poll_for_loop`]) so an abandoned
    /// session is torn down after [`MAX_RECORDING_DURATION`] even if no keyboard
    /// events ever arrive to drive it.
    fn recording_loop(app: AppHandle, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let step = match app.try_state::<HandyKeysState>() {
                Some(state) => state.recording.poll_for_loop(&running, Instant::now()),
                None => break,
            };

            match step {
                RecordingStep::Event(key_event) => {
                    // Convert to frontend-friendly format
                    let frontend_event = FrontendKeyEvent {
                        modifiers: modifiers_to_strings(key_event.modifiers),
                        key: key_event.key.map(|k| k.to_string().to_lowercase()),
                        is_key_down: key_event.is_key_down,
                        hotkey_string: key_event
                            .as_hotkey()
                            .map(|h| h.to_handy_string())
                            .unwrap_or_default(),
                    };

                    // Emit to frontend
                    if let Err(e) = app.emit("handy-keys-event", &frontend_event) {
                        error!("Failed to emit key event: {}", e);
                    }
                }
                RecordingStep::Idle => thread::sleep(Duration::from_millis(10)),
                RecordingStep::Stop => break,
            }
        }

        debug!("Recording loop ended");
    }

    /// Stop recording mode (idempotent full teardown).
    pub fn stop_recording(&self) -> Result<(), String> {
        self.recording.teardown();
        debug!("Stopped handy-keys recording mode");
        Ok(())
    }
}

impl Drop for HandyKeysState {
    fn drop(&mut self) {
        // Fully tear down any in-flight recording session (stops the loop and
        // drops the listener).
        self.recording.teardown();

        // Send shutdown command
        if let Ok(sender) = self.command_sender.lock() {
            let _ = sender.send(ManagerCommand::Shutdown);
        }

        // Wait for the manager thread to finish
        if let Ok(mut handle) = self.thread_handle.lock() {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// Convert handy-keys Modifiers to a list of strings
fn modifiers_to_strings(modifiers: handy_keys::Modifiers) -> Vec<String> {
    let mut result = Vec::new();

    if modifiers.contains(handy_keys::Modifiers::CTRL) {
        result.push("ctrl".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::OPT) {
        #[cfg(target_os = "macos")]
        result.push("option".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("alt".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::SHIFT) {
        result.push("shift".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::CMD) {
        #[cfg(target_os = "macos")]
        result.push("command".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("super".to_string());
    }
    if modifiers.contains(handy_keys::Modifiers::FN) {
        result.push("fn".to_string());
    }

    result
}

/// Validate a shortcut string for the HandyKeys implementation.
/// HandyKeys is more permissive: allows modifier-only combos and the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    // HandyKeys accepts modifier-only, key-only, and modifier+key combos
    // Just verify the string is parseable
    raw.parse::<Hotkey>()
        .map(|_| ())
        .map_err(|e| format!("Invalid shortcut for HandyKeys: {}", e))
}

/// Initialize handy-keys shortcuts
pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    let state = HandyKeysState::new(app.clone())?;

    let default_bindings = settings::get_default_settings().bindings;
    let user_settings = settings::load_or_create_app_settings(app);

    // Register all bindings except cancel (which is dynamic)
    for (id, default_binding) in default_bindings {
        if id == "cancel" {
            continue;
        }
        // Skip post-processing shortcut when the feature is disabled
        if id == "transcribe_with_post_process" && !user_settings.post_process_enabled {
            continue;
        }

        let binding = user_settings
            .bindings
            .get(&id)
            .cloned()
            .unwrap_or(default_binding);

        if binding.current_binding.trim().is_empty() {
            continue;
        }

        if let Err(e) = state.register(&binding) {
            error!(
                "Failed to register handy-keys shortcut {} during init: {}",
                id, e
            );
        }
    }

    app.manage(state);
    info!("handy-keys shortcuts initialized");
    Ok(())
}

/// Register the cancel shortcut (called when recording starts)
pub fn register_cancel_shortcut(app: &AppHandle) {
    // Disabled on Linux due to instability
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(cancel_binding) = get_settings(&app_clone).bindings.get("cancel").cloned() {
                if let Some(state) = app_clone.try_state::<HandyKeysState>() {
                    if let Err(e) = state.register(&cancel_binding) {
                        error!("Failed to register cancel shortcut: {}", e);
                    }
                }
            }
        });
    }
}

/// Unregister the cancel shortcut (called when recording stops)
pub fn unregister_cancel_shortcut(app: &AppHandle) {
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let app_clone = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(cancel_binding) = get_settings(&app_clone).bindings.get("cancel").cloned() {
                if let Some(state) = app_clone.try_state::<HandyKeysState>() {
                    let _ = state.unregister(&cancel_binding);
                }
            }
        });
    }
}

/// Register an action shortcut (bare digit key, called when recording starts)
pub fn register_action_shortcut(app: &AppHandle, binding: ShortcutBinding) {
    #[cfg(target_os = "linux")]
    {
        let _ = (app, binding);
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let app_clone = app.clone();
        let binding_clone = binding;
        tauri::async_runtime::spawn(async move {
            if let Some(state) = app_clone.try_state::<HandyKeysState>() {
                if let Err(e) = state.register(&binding_clone) {
                    error!(
                        "Failed to register action shortcut '{}': {}",
                        binding_clone.id, e
                    );
                }
            }
        });
    }
}

/// Unregister an action shortcut (called when recording stops)
pub fn unregister_action_shortcut(app: &AppHandle, binding: ShortcutBinding) {
    #[cfg(target_os = "linux")]
    {
        let _ = (app, binding);
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let app_clone = app.clone();
        let binding_clone = binding;
        tauri::async_runtime::spawn(async move {
            if let Some(state) = app_clone.try_state::<HandyKeysState>() {
                let _ = state.unregister(&binding_clone);
            }
        });
    }
}

/// Register a shortcut
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.register(&binding)
}

/// Unregister a shortcut
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.unregister(&binding)
}

/// Start key recording mode
#[tauri::command]
#[specta::specta]
pub fn start_handy_keys_recording(app: AppHandle, binding_id: String) -> Result<(), String> {
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.start_recording(&app, binding_id)
}

/// Stop key recording mode
#[tauri::command]
#[specta::specta]
pub fn stop_handy_keys_recording(app: AppHandle) -> Result<(), String> {
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.stop_recording()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkey(s: &str) -> Hotkey {
        s.parse().expect("valid hotkey")
    }

    #[test]
    fn should_only_block_transcription_bindings() {
        // Push-to-talk transcribe triggers must be suppressed at the OS level so
        // that holding e.g. `option+space` does not type spaces into the focused
        // app. Every other binding must stay passive so it never swallows keys
        // system-wide.
        assert!(should_block_os_key("transcribe"));
        assert!(should_block_os_key("transcribe_with_post_process"));

        assert!(!should_block_os_key("pause"));
        assert!(!should_block_os_key("cancel"));
        assert!(!should_block_os_key("show_history"));
        assert!(!should_block_os_key("copy_latest_history"));
        // Post-processing / action bindings (e.g. digit actions) must not block.
        assert!(!should_block_os_key("action_1"));
        assert!(!should_block_os_key("action_9"));
        assert!(!should_block_os_key("ppa-example"));
    }

    // ---------------------------------------------------------------------
    // Canonical global physical hotkey registry (inter-manager collisions).
    // Uses a `u32` id because `HotkeyId` cannot be constructed outside the
    // handy-keys crate; the collision/cleanup logic is id-type agnostic.
    // ---------------------------------------------------------------------

    #[test]
    fn rejects_inter_manager_collision_blocking_then_passive() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");

        registry.check_collision(&combo, "transcribe").unwrap();
        registry.commit(
            "transcribe",
            combo,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        // A passive binding claiming the same physical combo is rejected.
        assert!(registry.check_collision(&combo, "pause").is_err());
    }

    #[test]
    fn rejects_inter_manager_collision_passive_then_blocking() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");

        registry.commit("pause", combo, "ctrl+space", 1, ManagerCategory::Passive);

        // A blocking binding claiming the same physical combo is rejected.
        assert!(registry.check_collision(&combo, "transcribe").is_err());
    }

    #[test]
    fn same_binding_may_recheck_its_own_hotkey() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");

        registry.commit(
            "transcribe",
            combo,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        // Re-checking the same binding/combo is not a collision.
        assert!(registry.check_collision(&combo, "transcribe").is_ok());
    }

    #[test]
    fn unregister_then_reregister_cleans_registry() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");

        registry.commit(
            "transcribe",
            combo,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );
        let removed = registry.remove("transcribe");
        assert_eq!(removed, Some((1, ManagerCategory::Blocking)));

        // The combo is free again and can be claimed by a different binding, even
        // in the other manager category.
        assert!(registry.check_collision(&combo, "pause").is_ok());
        registry.commit("pause", combo, "ctrl+space", 7, ManagerCategory::Passive);
        assert_eq!(
            registry
                .dispatch_target(ManagerCategory::Passive, &7)
                .map(|(b, _)| b.as_str()),
            Some("pause")
        );
        // The old blocking id no longer dispatches to anything.
        assert!(registry
            .dispatch_target(ManagerCategory::Blocking, &1)
            .is_none());
    }

    #[test]
    fn failed_registration_does_not_pollute_registry() {
        let registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");

        // Collision check passes, then the OS manager (would) reject the hotkey,
        // so `commit` is never called.
        registry.check_collision(&combo, "transcribe").unwrap();

        // The registry is untouched: nothing tracked and the combo is still free.
        assert!(registry.by_hotkey.is_empty());
        assert!(registry.by_binding.is_empty());
        assert!(registry.check_collision(&combo, "pause").is_ok());
    }

    // ---------------------------------------------------------------------
    // Transactional register/replace and unregister (mock OS via closures).
    // These exercise the stage/check → commit-or-preserve ordering without a
    // real `HotkeyManager` (whose `HotkeyId` cannot be constructed here).
    // ---------------------------------------------------------------------

    #[test]
    fn reregistering_same_combo_is_noop() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");
        registry.commit(
            "transcribe",
            combo,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        // An identical re-registration must not touch the OS at all.
        registry
            .register_transactional(
                "transcribe",
                combo,
                "ctrl+space",
                ManagerCategory::Blocking,
                |_| panic!("no OS register for an identical re-registration"),
                |_, _| panic!("no OS unregister for an identical re-registration"),
            )
            .unwrap();
        assert_eq!(
            registry
                .dispatch_target(ManagerCategory::Blocking, &1)
                .map(|(b, _)| b.as_str()),
            Some("transcribe")
        );
    }

    #[test]
    fn replacement_colliding_with_other_binding_preserves_old() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let a = hotkey("ctrl+space");
        let b = hotkey("ctrl+alt+p");
        registry.commit("transcribe", a, "ctrl+space", 1, ManagerCategory::Blocking);
        registry.commit("pause", b, "ctrl+alt+p", 2, ManagerCategory::Passive);

        // "transcribe" tries to move onto "pause"'s combo: rejected up front, so
        // neither the OS nor the registry is touched.
        let res = registry.register_transactional(
            "transcribe",
            b,
            "ctrl+alt+p",
            ManagerCategory::Blocking,
            |_| panic!("OS register must not run on a rejected collision"),
            |_, _| panic!("OS unregister must not run on a rejected collision"),
        );
        assert!(res.is_err());

        // "transcribe" still owns its original combo and dispatches to its id.
        assert_eq!(
            registry.by_hotkey.get(&a).map(String::as_str),
            Some("transcribe")
        );
        assert_eq!(
            registry
                .dispatch_target(ManagerCategory::Blocking, &1)
                .map(|(x, _)| x.as_str()),
            Some("transcribe")
        );
    }

    #[test]
    fn failed_replacement_registration_preserves_old_binding() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let old = hotkey("ctrl+space");
        let new = hotkey("ctrl+alt+p");
        registry.commit(
            "transcribe",
            old,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        // The new combo passes the collision check but the OS registration fails.
        let res = registry.register_transactional(
            "transcribe",
            new,
            "ctrl+alt+p",
            ManagerCategory::Blocking,
            |_| Err("os rejected".to_string()),
            |_, _| panic!("old registration must be preserved, not released, on failure"),
        );
        assert!(res.is_err());

        // The old registration — combo, dispatch id, and canonical ownership — is
        // fully intact; the new combo was never recorded.
        assert_eq!(
            registry.by_hotkey.get(&old).map(String::as_str),
            Some("transcribe")
        );
        assert!(registry.by_hotkey.get(&new).is_none());
        assert_eq!(
            registry
                .dispatch_target(ManagerCategory::Blocking, &1)
                .map(|(b, _)| b.as_str()),
            Some("transcribe")
        );
    }

    #[test]
    fn successful_replacement_swaps_combo_and_releases_old() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let old = hotkey("ctrl+space");
        let new = hotkey("ctrl+alt+p");
        registry.commit(
            "transcribe",
            old,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        let mut released = None;
        registry
            .register_transactional(
                "transcribe",
                new,
                "ctrl+alt+p",
                ManagerCategory::Blocking,
                |_| Ok(2u32),
                |id, cat| {
                    released = Some((id, cat));
                    Ok(())
                },
            )
            .unwrap();

        // The old combo is released from the OS only after the new one is live.
        assert_eq!(released, Some((1, ManagerCategory::Blocking)));
        assert!(registry.by_hotkey.get(&old).is_none());
        assert_eq!(
            registry.by_hotkey.get(&new).map(String::as_str),
            Some("transcribe")
        );
        assert_eq!(
            registry
                .dispatch_target(ManagerCategory::Blocking, &2)
                .map(|(b, _)| b.as_str()),
            Some("transcribe")
        );
        assert!(registry
            .dispatch_target(ManagerCategory::Blocking, &1)
            .is_none());
    }

    #[test]
    fn failed_os_unregister_keeps_registry_entry() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");
        registry.commit(
            "transcribe",
            combo,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        // The OS unregister fails: the canonical registry must retain the entry so
        // it never claims a still-registered combo is free.
        let res =
            registry.unregister_transactional("transcribe", |_, _| Err("os busy".to_string()));
        assert!(res.is_err());
        assert_eq!(
            registry.by_hotkey.get(&combo).map(String::as_str),
            Some("transcribe")
        );
        assert!(registry.os_handle("transcribe").is_some());
        // A different binding still cannot claim the combo.
        assert!(registry.check_collision(&combo, "pause").is_err());
    }

    #[test]
    fn successful_os_unregister_frees_combo() {
        let mut registry = HotkeyRegistry::<u32>::default();
        let combo = hotkey("ctrl+space");
        registry.commit(
            "transcribe",
            combo,
            "ctrl+space",
            1,
            ManagerCategory::Blocking,
        );

        registry
            .unregister_transactional("transcribe", |id, cat| {
                assert_eq!((id, cat), (1, ManagerCategory::Blocking));
                Ok(())
            })
            .unwrap();
        assert!(registry.by_hotkey.get(&combo).is_none());
        assert!(registry.os_handle("transcribe").is_none());
        assert!(registry.check_collision(&combo, "pause").is_ok());
    }

    #[test]
    fn unregister_unknown_binding_is_ok() {
        let mut registry = HotkeyRegistry::<u32>::default();
        registry
            .unregister_transactional("missing", |_, _| panic!("no OS call for unknown binding"))
            .unwrap();
    }

    // ---------------------------------------------------------------------
    // Recording session lifecycle (no real OS hooks: `listener: None`).
    // The whole lifecycle is behind one mutex, so transitions are linearizable.
    // ---------------------------------------------------------------------

    /// Claim and install a fresh session, returning its per-session stop flag.
    fn claim_and_install(state: &RecordingState, binding: &str) -> Arc<AtomicBool> {
        let generation = state.claim(Instant::now()).expect("claim succeeds");
        state
            .install_session(generation, binding.into(), None, Instant::now())
            .expect("install succeeds for a fresh claim")
    }

    #[test]
    fn rejects_concurrent_double_start() {
        let state = RecordingState::new();
        let _g = state.claim(Instant::now()).expect("first claim succeeds");
        // A second claim while one is in flight must be rejected, not replace it.
        assert!(state.claim(Instant::now()).is_err());
    }

    #[test]
    fn teardown_between_claim_and_install_drops_stale_listener() {
        let state = RecordingState::new();
        let generation = state.claim(Instant::now()).expect("claim");

        // A teardown races in after the claim but before the session is installed.
        state.teardown();

        // The now-stale install must be refused, so the listener is dropped rather
        // than installed while the lifecycle believes it is idle.
        assert!(state
            .install_session(generation, "transcribe".into(), None, Instant::now())
            .is_none());
        assert!(!state.is_capturing());
        // The slot is coherently free to claim again.
        assert!(state.claim(Instant::now()).is_ok());
    }

    #[test]
    fn install_for_superseded_claim_is_refused() {
        let state = RecordingState::new();
        let stale = state.claim(Instant::now()).expect("first claim");
        state.teardown();
        // A newer claim takes a fresh generation.
        let _fresh = state.claim(Instant::now()).expect("second claim");

        // The stale claim's install must not clobber the newer claim.
        assert!(state
            .install_session(stale, "transcribe".into(), None, Instant::now())
            .is_none());
    }

    #[test]
    fn expiry_fully_and_idempotently_cleans_session() {
        let state = RecordingState::new();
        let running = claim_and_install(&state, "transcribe");

        // A fresh session is still capturing.
        assert!(state.is_capturing_at(Instant::now()));

        // Past the max duration, capture expires AND everything is cleared, so no
        // stale listener/flag survives while global actions resume.
        let future = Instant::now() + MAX_RECORDING_DURATION + Duration::from_secs(1);
        assert!(!state.is_capturing_at(future));
        assert!(!running.load(Ordering::SeqCst), "expiry stops the loop");
        // A new session can be claimed again after expiry.
        assert!(state.claim(Instant::now()).is_ok());
    }

    #[test]
    fn watchdog_expires_session_without_any_events() {
        let state = RecordingState::new();
        let running = claim_and_install(&state, "transcribe");

        // A fresh poll keeps the loop alive (idle: no listener, no event).
        assert!(matches!(
            state.poll_for_loop(&running, Instant::now()),
            RecordingStep::Idle
        ));

        // Past the max duration the watchdog tears the session down from the
        // loop's own tick — no keyboard event and no `is_capturing()` call needed.
        let future = Instant::now() + MAX_RECORDING_DURATION + Duration::from_secs(1);
        assert!(matches!(
            state.poll_for_loop(&running, future),
            RecordingStep::Stop
        ));
        assert!(!running.load(Ordering::SeqCst), "watchdog stopped the loop");
        assert!(!state.is_capturing());
    }

    #[test]
    fn poll_for_loop_stops_superseded_loop() {
        let state = RecordingState::new();
        let old = claim_and_install(&state, "transcribe");
        state.teardown();
        let new = claim_and_install(&state, "pause");

        // The old loop must stop — it no longer owns the live session ...
        assert!(matches!(
            state.poll_for_loop(&old, Instant::now()),
            RecordingStep::Stop
        ));
        // ... while the loop that owns the current session keeps running.
        assert!(matches!(
            state.poll_for_loop(&new, Instant::now()),
            RecordingStep::Idle
        ));
    }

    #[test]
    fn teardown_clears_all_state() {
        let state = RecordingState::new();
        let running = claim_and_install(&state, "pause");

        state.teardown();

        assert!(!running.load(Ordering::SeqCst), "loop stop flag cleared");
        assert!(!state.is_capturing());
    }

    #[test]
    fn teardown_is_idempotent() {
        let state = RecordingState::new();
        let _running = claim_and_install(&state, "pause");

        state.teardown();
        // A second teardown must not panic and must leave clean state.
        state.teardown();

        assert!(!state.is_capturing());
        assert!(state.claim(Instant::now()).is_ok());
    }

    #[test]
    fn concurrent_teardown_and_claim_never_corrupt_state() {
        use std::sync::Barrier;

        // Hammer the claim↔teardown boundary from two real threads. With the
        // single-mutex lifecycle this can only ever settle into a coherent phase;
        // the old split AtomicBool + Mutex design could leave "recording = true"
        // with no session (or a live listener with recording = false).
        for _ in 0..500 {
            let state = Arc::new(RecordingState::new());
            // Start from a live session that the teardown thread will tear down
            // while the claim thread races to start a fresh one.
            let _ = claim_and_install(&state, "transcribe");

            let barrier = Arc::new(Barrier::new(2));

            let s1 = Arc::clone(&state);
            let b1 = Arc::clone(&barrier);
            let t_teardown = thread::spawn(move || {
                b1.wait();
                s1.teardown();
            });

            let s2 = Arc::clone(&state);
            let b2 = Arc::clone(&barrier);
            let t_claim = thread::spawn(move || {
                b2.wait();
                // May win (teardown already ran) or lose (session still live);
                // both outcomes are valid. On a win, complete the install handoff.
                if let Ok(generation) = s2.claim(Instant::now()) {
                    s2.install_session(generation, "pause".into(), None, Instant::now());
                }
            });

            t_teardown.join().unwrap();
            t_claim.join().unwrap();

            // Invariant: `is_capturing()` and the actual phase always agree — the
            // flag can never be set with a missing session, or vice versa.
            let capturing = state.is_capturing();
            let inner = state.lock();
            match &inner.phase {
                RecordingPhase::Idle => assert!(!capturing),
                RecordingPhase::Claimed(_) | RecordingPhase::Active(_) => assert!(capturing),
            }
            drop(inner);

            state.teardown();
        }
    }
}
