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
//! polled from a dedicated recording thread. Events are emitted to the frontend
//! via Tauri's event system. The whole capture session lives behind a single
//! [`RecordingState`] with an atomic claim (no concurrent double-start) and an
//! idempotent teardown that also auto-expires (see [`MAX_RECORDING_DURATION`]).

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
    running: Arc<AtomicBool>,
}

/// Single source of truth for the settings-UI key capture session.
///
/// `is_recording` is the atomic guard: `claim` flips it `false → true` with a
/// `compare_exchange`, so two concurrent `start_recording` calls can never both
/// win. `session` holds the listener, timing and per-session stop flag. Every
/// teardown path clears all of it, idempotently.
struct RecordingState {
    is_recording: AtomicBool,
    session: Mutex<Option<RecordingSession>>,
}

impl RecordingState {
    fn new() -> Self {
        Self {
            is_recording: AtomicBool::new(false),
            session: Mutex::new(None),
        }
    }

    /// Atomically claim the recording slot.
    ///
    /// A stale (expired) session is torn down first so a crashed recording can't
    /// block new ones. Returns `Err` if another session is already active. The
    /// `compare_exchange` makes this safe against concurrent double-start.
    fn claim(&self) -> Result<(), String> {
        self.expire_if_stale(Instant::now());
        self.is_recording
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "Already recording".to_string())?;
        Ok(())
    }

    /// Release a claim taken by [`claim`](Self::claim) when session setup fails
    /// before a session is installed.
    fn abort_claim(&self) {
        self.is_recording.store(false, Ordering::SeqCst);
    }

    /// Install the live session and return its per-session stop flag for the
    /// recording loop. Must be called after a successful [`claim`](Self::claim).
    fn install_session(
        &self,
        binding_id: String,
        listener: Option<KeyboardListener>,
    ) -> Arc<AtomicBool> {
        let running = Arc::new(AtomicBool::new(true));
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(RecordingSession {
            listener,
            binding_id,
            started_at: Instant::now(),
            running: Arc::clone(&running),
        });
        running
    }

    /// Fully and idempotently tear down any session: stop the loop, drop the
    /// listener, and clear the flag, binding id, and timestamp.
    fn teardown(&self) {
        self.is_recording.store(false, Ordering::SeqCst);
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.as_ref() {
            session.running.store(false, Ordering::SeqCst);
        }
        *guard = None;
    }

    /// If a session has outlived [`MAX_RECORDING_DURATION`], tear it down.
    /// Returns whether an expiry-driven teardown happened. `now` is injected so
    /// expiry is testable without sleeping.
    fn expire_if_stale(&self, now: Instant) -> bool {
        if !self.is_recording.load(Ordering::SeqCst) {
            return false;
        }
        let stale = {
            let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(session) => {
                    now.saturating_duration_since(session.started_at) >= MAX_RECORDING_DURATION
                }
                // Claimed but session not yet installed: mid-setup, not stale.
                None => false,
            }
        };
        if stale {
            self.teardown();
        }
        stale
    }

    /// Whether the UI is actively capturing keys for a new binding.
    ///
    /// Returns false (and fully tears the session down) once the session has
    /// outlived [`MAX_RECORDING_DURATION`], so a frontend that never calls
    /// `stop_recording` can't leave global shortcuts suppressed or a stale
    /// listener alive.
    fn is_capturing(&self) -> bool {
        self.is_capturing_at(Instant::now())
    }

    fn is_capturing_at(&self, now: Instant) -> bool {
        if !self.is_recording.load(Ordering::SeqCst) {
            return false;
        }
        if self.expire_if_stale(now) {
            return false;
        }
        true
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

        // If this binding is already registered (e.g. re-register without an
        // explicit unregister), tear the old entry down first so the canonical
        // registry never leaks a stale combo or OS registration.
        if registry.by_binding.contains_key(binding_id) {
            Self::do_unregister(registry, blocking_manager, passive_manager, binding_id)?;
        }

        // Atomically reject a combo already owned by another binding, in either
        // registration order, before any OS manager is touched.
        registry.check_collision(&hotkey, binding_id)?;

        let category = physical_category(binding_id);
        let manager = match category {
            ManagerCategory::Blocking => blocking_manager.unwrap_or(passive_manager),
            ManagerCategory::Passive => passive_manager,
        };

        // On failure the registry is left untouched (commit runs only on success).
        let id = manager
            .register(hotkey)
            .map_err(|e| format!("Failed to register hotkey: {}", e))?;

        registry.commit(binding_id, hotkey, hotkey_string, id, category);

        debug!(
            "Registered handy-keys shortcut: {} -> {:?} ({:?})",
            binding_id, hotkey, category
        );
        Ok(())
    }

    /// Unregister a hotkey, cleaning every registry map even if the OS
    /// unregister errors.
    fn do_unregister(
        registry: &mut HotkeyRegistry,
        blocking_manager: Option<&HotkeyManager>,
        passive_manager: &HotkeyManager,
        binding_id: &str,
    ) -> Result<(), String> {
        if let Some((id, category)) = registry.remove(binding_id) {
            let manager = match category {
                ManagerCategory::Blocking => blocking_manager.unwrap_or(passive_manager),
                ManagerCategory::Passive => passive_manager,
            };
            manager
                .unregister(id)
                .map_err(|e| format!("Failed to unregister hotkey: {}", e))?;
            debug!("Unregistered handy-keys shortcut: {}", binding_id);
        }
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
        self.recording.claim()?;

        // Create a new keyboard listener for recording. If this fails we must
        // release the claim so recording isn't wedged "on" forever.
        let listener = match KeyboardListener::new() {
            Ok(l) => l,
            Err(e) => {
                self.recording.abort_claim();
                return Err(format!("Failed to create keyboard listener: {}", e));
            }
        };

        let running = self.recording.install_session(binding_id, Some(listener));

        // Start a thread to emit key events to the frontend
        let app_clone = app.clone();
        thread::spawn(move || {
            Self::recording_loop(app_clone, running);
        });

        debug!("Started handy-keys recording mode");
        Ok(())
    }

    /// Recording loop - emits key events to frontend during recording
    fn recording_loop(app: AppHandle, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let event = {
                let state = match app.try_state::<HandyKeysState>() {
                    Some(s) => s,
                    None => break,
                };
                let guard = state.recording.session.lock().ok();
                guard.and_then(|g| g.as_ref()?.listener.as_ref()?.try_recv())
            };

            if let Some(key_event) = event {
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
            } else {
                thread::sleep(Duration::from_millis(10));
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
        let mut registry = HotkeyRegistry::<u32>::default();
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
    // Recording session lifecycle (no real OS hooks: `listener: None`).
    // ---------------------------------------------------------------------

    #[test]
    fn rejects_concurrent_double_start() {
        let state = RecordingState::new();
        state.claim().expect("first claim succeeds");
        // A concurrent second claim must be rejected, not replace the session.
        assert!(state.claim().is_err());
    }

    #[test]
    fn expiry_fully_and_idempotently_cleans_session() {
        let state = RecordingState::new();
        state.claim().unwrap();
        state.install_session("transcribe".into(), None);

        // A fresh session is still capturing.
        assert!(state.is_capturing_at(Instant::now()));

        // Past the max duration, capture expires AND everything is cleared, so
        // no stale listener/flag survives while global actions resume.
        let future = Instant::now() + MAX_RECORDING_DURATION + Duration::from_secs(1);
        assert!(!state.is_capturing_at(future));
        assert!(!state.is_recording.load(Ordering::SeqCst));
        assert!(state.session.lock().unwrap().is_none());

        // A new session can be claimed again after expiry.
        assert!(state.claim().is_ok());
    }

    #[test]
    fn teardown_clears_all_state() {
        let state = RecordingState::new();
        state.claim().unwrap();
        let running = state.install_session("pause".into(), None);

        state.teardown();

        assert!(!state.is_recording.load(Ordering::SeqCst));
        assert!(!running.load(Ordering::SeqCst), "loop stop flag cleared");
        assert!(state.session.lock().unwrap().is_none());
    }

    #[test]
    fn teardown_is_idempotent() {
        let state = RecordingState::new();
        state.claim().unwrap();
        state.install_session("pause".into(), None);

        state.teardown();
        // A second teardown must not panic and must leave clean state.
        state.teardown();

        assert!(!state.is_recording.load(Ordering::SeqCst));
        assert!(state.session.lock().unwrap().is_none());
        assert!(!state.is_capturing());
    }
}
