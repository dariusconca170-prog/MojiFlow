//! Application state machine and the UI ↔ worker message bus.
//!
//! `UiCommand` flows UI → workers, `CoreEvent` flows workers → UI through bounded channels.
//! Workers hold a [`RepaintHandle`] and signal it after posting an event so the UI wakes
//! up exactly when there is something new (never busy-polling).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::capture::AudioCapture;
use crate::capture::window::GlobalPointer;
use crate::clock::{self, manual::ManualClock, PlaybackClock};
use crate::config::{ClockSource, Config};
use crate::dict::Dictionary;
use crate::error::MlError;
use crate::export::{mine_card, ExportSource, ExportWorker};
use crate::gui::dashboard::TokenRow;
use crate::gui::popover::PopoverData;
use crate::hotkey::{AppAction, GlobalHotkeys};
use crate::subs::SubtitleTrack;
use crate::tokenize::{JapaneseTokenizer, Token};

/// Commands sent from the UI thread to worker threads/tasks.
#[derive(Debug)]
pub enum UiCommand {
    /// Load a subtitle track from a path (drag-and-drop or file dialog).
    LoadSubtitles(std::path::PathBuf),
    /// Stop the worker and release resources.
    Shutdown,
}

/// Events posted by workers to the UI thread.
#[derive(Debug)]
pub enum CoreEvent {
    /// A subtitle track finished loading (or failed with a typed error).
    TrackLoaded(Result<SubtitleTrack, MlError>),
    /// A subsystem reported a human-readable status change (status strip).
    Status {
        component: &'static str,
        text: String,
    },
    /// A non-fatal error to surface as a toast.
    Warning(MlError),
    /// A local-LLM sentence explanation finished (or failed with a message).
    Explanation {
        key: String,
        result: Result<String, String>,
    },
    /// A fatal-but-recoverable error; the app keeps running, the worker stopped.
    WorkerFailed {
        worker: &'static str,
        error: MlError,
    },
}

/// Worker-side handle onto the renderer: lets background threads wake the UI without
/// owning the full `egui::Context` API surface.
pub struct RepaintHandle {
    ctx: egui::Context,
}

impl RepaintHandle {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx }
    }

    /// Standalone context for worker tests that must run without a window.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            ctx: egui::Context::default(),
        }
    }

    /// Wake the UI immediately.
    pub fn request(&self) {
        self.ctx.request_repaint();
    }

    /// Wake the UI at most after `delay` — used to throttle high-frequency sources
    /// (mpv time-pos) so idle CPU stays low.
    pub fn request_after(&self, delay: Duration) {
        self.ctx.request_repaint_after(delay);
    }
}

impl Clone for RepaintHandle {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
        }
    }
}

/// Auto-hiding toast shown in a corner of the overlay.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub is_error: bool,
    pub created: Instant,
}

const TOAST_TTL: Duration = Duration::from_secs(4);
/// Errors stay readable longer — a 4 s toast for an X11 failure cannot be read in time
/// (user review 2026-10-10).
const ERROR_TOAST_TTL: Duration = Duration::from_secs(12);
const MAX_TOASTS: usize = 6;

/// Toast queue with TTL expiry.
#[derive(Debug, Default)]
pub struct ToastQueue {
    items: VecDeque<Toast>,
}

impl ToastQueue {
    pub fn push(&mut self, message: impl Into<String>, is_error: bool) {
        self.items.push_back(Toast {
            message: message.into(),
            is_error,
            created: Instant::now(),
        });
        while self.items.len() > MAX_TOASTS {
            self.items.pop_front();
        }
    }

    pub fn expire(&mut self, now: Instant) {
        self.items.retain(|t| {
            let ttl = if t.is_error {
                ERROR_TOAST_TTL
            } else {
                TOAST_TTL
            };
            now.duration_since(t.created) < ttl
        });
    }

    pub fn iter(&self) -> impl Iterator<Item = &Toast> {
        self.items.iter()
    }
}

/// Bounded event channel wiring handed to every worker.
#[derive(Clone)]
pub struct EventBus {
    tx: crossbeam_channel::Sender<CoreEvent>,
    repaint: Arc<RepaintHandle>,
}

impl EventBus {
    pub fn new(tx: crossbeam_channel::Sender<CoreEvent>, repaint: Arc<RepaintHandle>) -> Self {
        Self { tx, repaint }
    }

    /// Post an event and wake the UI. Never blocks when the channel is full — a dropped
    /// event is preferable to stalling a worker.
    pub fn post(&self, event: CoreEvent) {
        if self.tx.send(event).is_ok() {
            self.repaint.request();
        }
    }
}

/// The eframe application state for the overlay window.
pub struct App {
    /// Active configuration (mutated by the settings panel, persisted on change).
    pub config: Config,
    pub config_path: Option<std::path::PathBuf>,
    /// Issues found while loading config; shown once as a toast on first frame.
    pub config_issues: Vec<MlError>,
    pub toasts: ToastQueue,
    /// Whether the CJK font loaded from the system or the embedded fallback.
    pub font_source: crate::gui::fonts::FontSource,
    /// Repaint handle for workers built after startup (clock switching).
    repaint: Arc<RepaintHandle>,
    /// Milestone-1 demo content, shown until a track is loaded and a cue is active.
    pub demo_text: String,

    // --- M2: subtitles + clocks ---
    pub track: Option<SubtitleTrack>,
    /// Index of the currently active cue (into `track`).
    pub active_cue: Option<usize>,
    /// User fine offset in ms (`[`/`]`), layered on top of the clock.
    pub user_offset_ms: i64,
    /// Current media-time source (manual / mpv / mpris). Workers never touch it.
    pub clock: Box<dyn PlaybackClock>,
    /// Control handle for the manual clock (shares state with `clock` when manual).
    pub manual: ManualClock,
    events_rx: crossbeam_channel::Receiver<CoreEvent>,
    /// Sender half, kept so on-demand workers (explain) can post results.
    events_tx: crossbeam_channel::Sender<CoreEvent>,
    loader_tx: crossbeam_channel::Sender<UiCommand>,
    /// Workers that must be joined/stopped at exit.
    shutdown_hooks: Vec<Box<dyn FnOnce() + Send>>,
    last_sync_state: Option<crate::clock::SyncState>,

    // --- M4: interactive overlay (click-through, hover, dictionary popover) ---
    /// Dictionary opened from config; `None` when its database is missing (toast once).
    pub dictionary: Option<Dictionary>,
    /// IPADIC tokenizer, built once (decoding the embedded dictionary is not free).
    tokenizer: Option<JapaneseTokenizer>,
    /// OS pointer/modifier probe used to detect hover while the window is click-through.
    pointer: GlobalPointer,
    /// Latest window-local cursor position in egui points, if the OS could report it.
    pub cursor_local: Option<egui::Pos2>,
    /// Whether Shift is currently held (queried globally) — the "Shift-lock".
    pub shift_held: bool,
    /// Interactive regions computed during the previous frame (tokens, popover, status).
    pub interactive_rects: Vec<egui::Rect>,
    /// True while the window currently accepts mouse input (hittest enabled).
    pub hit_testing: bool,
    /// Token under the cursor this frame.
    pub hover_token: Option<usize>,
    /// Token pinned by a click; its popover stays open when the cursor moves away.
    pub pinned_token: Option<usize>,
    /// The popover currently shown, if any.
    pub popover: Option<PopoverState>,
    /// Tokenization cache keyed by the source text.
    token_cache: Option<(String, Arc<Vec<Token>>)>,
    /// Whether the pointer-unavailable warning has been surfaced to the user.
    pointer_warned: bool,

    // --- M5: loopback audio capture ---
    /// Background capture thread; `None` on the rare failure to spawn it.
    pub audio: Option<AudioCapture>,
    /// Last seen active capture identity (`device|rate|channels`) for transition toasts.
    audio_active_key: Option<String>,
    /// Last seen retry error, surfaced as a toast once per change.
    audio_last_error: Option<String>,

    // --- M7: global hotkeys + mining export ---
    /// Registered global hotkeys; `None` when manager init failed (local keys remain).
    hotkeys: Option<GlobalHotkeys>,
    /// Background exporter (own tokio runtime): media + Anki + offline queue.
    export_worker: Option<ExportWorker>,
    /// Offline queue directory (config dir + `/queue`).
    queue_dir: std::path::PathBuf,
    /// Lock: forces the overlay interactive everywhere (move/click) until unlocked.
    pub locked: bool,
    /// Visibility: `false` hides the whole overlay (hotkey Ctrl+Alt+H).
    pub visible: bool,
    /// Edit mode toggle (editing UI arrives with the M8 settings panel; state is shown
    /// in the status strip so the toggle is never a silent no-op).
    pub edit_mode: bool,
    /// Whether the status strip is drawn (hotkey Ctrl+Alt+P).
    pub show_status: bool,

    // --- M7 dashboard (control room) ---
    /// Last few worker `Status` lines (export results, sync notes) for the dashboard log.
    pub status_log: VecDeque<String>,
    /// Per-token dictionary resolution for the active cue, recomputed only when the cue
    /// text changes (the dashboard shows the "did we resolve it" state per word).
    pub dashboard_tokens: Option<(String, Vec<TokenRow>)>,
    /// Last hotkey action applied + when, shown in the dashboard header so key presses
    /// have visible feedback even while the overlay is click-through.
    pub last_action: Option<(Instant, &'static str)>,
    /// Last time the EWMH always-on-top hint was re-asserted (eframe drops the builder
    /// hint; we enforce `_NET_WM_STATE_ABOVE` ourselves at a low cadence).
    last_topmost_check: Instant,
    /// Only surface the always-on-top unsupported error once (e.g. macOS).
    topmost_error_logged: bool,
    /// Whether the clock has ever advanced (set on the first play/seek) — gates the
    /// overlay's "waiting for video" hint.
    pub ever_started: bool,
    /// True once the first-frame auto bottom-center placement has run.
    placed: bool,
    /// Whether fullscreen-follow currently has the overlay expanded to fullscreen.
    following_fullscreen: bool,
    /// Window rect to restore when fullscreen-follow exits.
    saved_window_rect: Option<[f32; 4]>,
    /// Last time the fullscreen-follow poll ran (same cadence as the topmost check).
    last_fullscreen_check: Instant,
    /// Whether the fullscreen-follow unsupported error was surfaced (once).
    fullscreen_logged: bool,
    /// "Start at" seconds typed into the dashboard clock card (user review 2026-10-10:
    /// "tell it how far into the video I am, then it waits for Space").
    pub start_at_seconds: String,
    /// Cached local-LLM explanations by `sentence｜term` key.
    pub explanations: HashMap<String, String>,
    /// Key of the in-flight explanation request, if any.
    pub explaining: Option<String>,
}

/// Open popover: which token it describes, the display model, and the rectangle it last
/// occupied (so it can stay open while the cursor is over the panel itself).
pub struct PopoverState {
    pub token_index: usize,
    pub data: PopoverData,
    pub rect: Option<egui::Rect>,
}

impl App {
    pub fn new(
        config: Config,
        config_path: Option<std::path::PathBuf>,
        config_issues: Vec<MlError>,
        font_source: crate::gui::fonts::FontSource,
        ctx: egui::Context,
    ) -> Self {
        let (events_tx, events_rx) = crossbeam_channel::bounded::<CoreEvent>(64);
        let repaint = Arc::new(RepaintHandle::new(ctx.clone()));
        let bus = EventBus::new(events_tx.clone(), Arc::clone(&repaint));
        // Clone now: `build_clock` moves `events_tx`, but the export worker (M7) needs it.
        let export_events = events_tx.clone();

        // Subtitle loader: a dedicated thread doing file I/O + parsing off the UI thread.
        let (loader_tx, loader_rx) = crossbeam_channel::bounded::<UiCommand>(8);
        let loader_bus = bus.clone();
        let loader_thread = std::thread::Builder::new()
            .name("subtitle-loader".to_owned())
            .spawn(move || {
                while let Ok(cmd) = loader_rx.recv() {
                    match cmd {
                        UiCommand::LoadSubtitles(path) => {
                            let result = crate::subs::load_path(&path).map_err(MlError::from);
                            loader_bus.post(CoreEvent::TrackLoaded(result));
                        }
                        UiCommand::Shutdown => break,
                    }
                }
            })
            .expect("spawn subtitle loader thread");
        let loader_join = Arc::new(std::sync::Mutex::new(Some(loader_thread)));

        let manual = ManualClock::new();
        let (clock, clock_fallback) = clock::build_clock(
            &config.clock,
            manual.clone(),
            events_tx,
            Arc::clone(&repaint),
        );

        let mut issues = config_issues;
        if let Some(err) = clock_fallback {
            issues.push(MlError::Clock(err));
        }

        // M4: dictionary + tokenizer. A missing JMdict DB is reported once as a toast but
        // must not stop the overlay from rendering subtitles.
        let dictionary = match Dictionary::open(&config.dictionary) {
            Ok(dict) => Some(dict),
            Err(err) => {
                issues.push(MlError::Dictionary(err));
                None
            }
        };
        let tokenizer = match JapaneseTokenizer::new() {
            Ok(tokenizer) => Some(tokenizer),
            Err(err) => {
                issues.push(MlError::Dictionary(err));
                None
            }
        };

        // M5: loopback audio capture into the "last-heard audio" ring. Starts on a dedicated
        // thread; retries and surfaces via `update_audio_state`.
        let audio = AudioCapture::start(&config.audio);

        // M7: global hotkeys. On native Wayland (or when the X manager fails) registration
        // errors surface once as a toast and the local keys stay as the fallback — a
        // graceful degradation, documented in AGENTS.md.
        let hotkeys = match GlobalHotkeys::new(&config.hotkeys) {
            Ok(hotkeys) => {
                tracing::info!(
                    count = hotkeys.registered_count(),
                    "global hotkeys registered"
                );
                Some(hotkeys)
            }
            Err(err) => {
                issues.push(MlError::Hotkey(err));
                None
            }
        };

        // M7: mining export — one worker thread with its own tokio runtime so the Anki
        // round trip (and media encoding) never touches the UI thread.
        let export_worker = ExportWorker::spawn(export_events.clone(), Arc::clone(&repaint));

        // Offline queue lives next to the config file; temp dir is the fallback when the
        // config path is unknown (e.g. default config without a persisted location).
        let queue_dir = config_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|dir| dir.join("queue"))
            .unwrap_or_else(|| std::env::temp_dir().join("medialingual-queue"));

        // A negative rect component is the "auto bottom-center" sentinel; compute before
        // `config` is moved into the struct.
        let placed = config.window.rect[0] >= 0.0 && config.window.rect[1] >= 0.0;

        Self {
            user_offset_ms: config.subtitle.user_offset_ms,
            config,
            config_path,
            config_issues: issues,
            toasts: ToastQueue::default(),
            font_source,
            repaint: Arc::clone(&repaint),
            demo_text: "日本語の文をマイニングしよう。漢字・かな・々・ー・〜 すべて表示されます。"
                .to_owned(),
            track: None,
            active_cue: None,
            clock,
            manual,
            events_rx,
            events_tx: export_events,
            loader_tx,
            shutdown_hooks: vec![Box::new(move || {
                if let Ok(mut guard) = loader_join.lock() {
                    if let Some(handle) = guard.take() {
                        let _ = handle.join();
                    }
                }
            })],
            last_sync_state: None,
            dictionary,
            tokenizer,
            pointer: GlobalPointer::new(),
            cursor_local: None,
            shift_held: false,
            interactive_rects: Vec::new(),
            hit_testing: false,
            hover_token: None,
            pinned_token: None,
            popover: None,
            token_cache: None,
            pointer_warned: false,
            audio: Some(audio),
            audio_active_key: None,
            audio_last_error: None,
            hotkeys,
            export_worker: Some(export_worker),
            queue_dir,
            locked: false,
            visible: true,
            edit_mode: false,
            show_status: true,
            status_log: VecDeque::new(),
            dashboard_tokens: None,
            last_action: None,
            last_topmost_check: Instant::now(),
            topmost_error_logged: false,
            ever_started: false,
            placed,
            following_fullscreen: false,
            saved_window_rect: None,
            last_fullscreen_check: Instant::now(),
            fullscreen_logged: false,
            start_at_seconds: String::new(),
            explanations: HashMap::new(),
            explaining: None,
        }
    }

    /// Surface config load issues exactly once, on the first frame.
    pub fn drain_config_issues(&mut self) {
        for issue in std::mem::take(&mut self.config_issues) {
            self.toasts.push(issue.to_string(), true);
        }
    }

    /// Drain worker events without blocking. Called every frame.
    pub fn poll_events(&mut self) {
        loop {
            match self.events_rx.try_recv() {
                Ok(CoreEvent::TrackLoaded(result)) => match result {
                    Ok(track) => {
                        self.toasts.push(
                            format!(
                                "loaded {} — {} cues ({}, {} skipped)",
                                track
                                    .path()
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                track.len(),
                                track.format().label(),
                                track.encoding(),
                            ),
                            false,
                        );
                        self.track = Some(track);
                        self.active_cue = None;
                    }
                    Err(err) => {
                        self.toasts.push(err.to_string(), true);
                        self.track = None;
                        self.active_cue = None;
                    }
                },
                Ok(CoreEvent::Status { component, text }) => {
                    tracing::info!(component, text = %text, "status");
                    self.status_log.push_back(text.clone());
                    while self.status_log.len() > 6 {
                        self.status_log.pop_front();
                    }
                }
                Ok(CoreEvent::Warning(err)) => {
                    self.toasts.push(err.to_string(), true);
                }
                Ok(CoreEvent::Explanation { key, result }) => {
                    self.explaining = None;
                    match result {
                        Ok(text) => {
                            if self.explanations.len() > 32 {
                                self.explanations.clear();
                            }
                            self.explanations.insert(key, text);
                        }
                        Err(err) => {
                            self.toasts.push(format!("explain failed: {err}"), true);
                        }
                    }
                }
                Ok(CoreEvent::WorkerFailed { worker, error }) => {
                    self.toasts.push(format!("{worker} stopped: {error}"), true);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// The text currently shown: active cue if any, otherwise nothing. There is
    /// deliberately no demo text on a fresh launch (user review 2026-10-10: the screen
    /// must stay clean until subtitles are loaded) — the dashboard explains how.
    pub fn subtitle_text(&self) -> String {
        match (&self.track, self.active_cue) {
            (Some(track), Some(idx)) => track.cue(idx).map(|c| c.text.clone()).unwrap_or_default(),
            _ => String::new(),
        }
    }

    pub fn subtitle_offset_ms(&self) -> i64 {
        self.user_offset_ms
    }

    // -------------------------------------------------------------------------------------
    // M4: tokenization cache, interactive hit-testing, dictionary popover
    // -------------------------------------------------------------------------------------

    /// Tokenize `text`, reusing the previous result when the text is unchanged. Tokenizing
    /// every frame would be wasteful; the cache key is the exact source text.
    pub fn tokens_cached(&mut self, text: &str) -> Arc<Vec<Token>> {
        if let Some((cached, tokens)) = &self.token_cache {
            if cached == text {
                return Arc::clone(tokens);
            }
        }
        let tokens = match &self.tokenizer {
            Some(tokenizer) => match tokenizer.tokenize(text) {
                Ok(tokens) => tokens,
                Err(err) => {
                    tracing::warn!(error = %err, "tokenization failed");
                    self.toasts.push(err.to_string(), true);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let arc = Arc::new(tokens);
        self.token_cache = Some((text.to_owned(), Arc::clone(&arc)));
        // A new sentence invalidates any hover/pin state tied to the previous token indices.
        self.hover_token = None;
        self.pinned_token = None;
        self.popover = None;
        arc
    }

    /// Query the OS for the pointer position (window-local) and Shift state. Safe to call
    /// every frame; failures surface once and then fall back to always-interactive.
    pub fn update_cursor(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        use raw_window_handle::HasWindowHandle as _;

        let ppp = ctx.pixels_per_point().max(1.0);
        let handle = frame.window_handle().ok().map(|handle| handle.as_raw());
        self.cursor_local = match handle {
            Some(handle) => match self.pointer.cursor(handle, ppp) {
                Ok(pos) => {
                    tracing::trace!(x = pos.x, y = pos.y, "global cursor");
                    Some(pos)
                }
                Err(err) => {
                    if !self.pointer_warned {
                        self.pointer_warned = true;
                        tracing::warn!(error = %err, "global cursor query unavailable");
                        self.toasts.push(err.to_string(), true);
                    }
                    None
                }
            },
            None => None,
        };
        self.shift_held = self.pointer.shift_held().unwrap_or(false);
    }

    /// Enable OS hit-testing exactly when the cursor is over an interactive region (or
    /// Shift-lock is held). Everything else stays click-through, so the video underneath
    /// keeps receiving clicks.
    pub fn apply_passthrough(&mut self, ctx: &egui::Context) {
        let interactive = match self.cursor_local {
            Some(pos) => {
                self.locked
                    || self.shift_held
                    || self.interactive_rects.iter().any(|rect| rect.contains(pos))
            }
            // No global cursor (native Wayland): the overlay must stay interactive so it can
            // still be used; manual-region mode is the M7 fallback. Documented in AGENTS.md.
            None => true,
        };
        if interactive != self.hit_testing {
            tracing::debug!(
                interactive,
                shift = self.shift_held,
                "toggling overlay mouse passthrough"
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(!interactive));
            self.hit_testing = interactive;
        }
    }

    /// Look up a token surface and build its popover model (dictionary may be absent).
    pub fn lookup_popover(&mut self, surface: &str) -> PopoverData {
        let resolution = match &mut self.dictionary {
            Some(dictionary) => match dictionary.resolve_detailed(surface) {
                Ok(resolution) => resolution,
                Err(err) => {
                    tracing::warn!(error = %err, surface, "dictionary lookup failed");
                    return PopoverData::from_resolution(surface, None);
                }
            },
            None => None,
        };
        PopoverData::from_resolution(surface, resolution.as_ref())
    }

    /// Recompute which popover (if any) should be shown for this frame.
    ///
    /// Selection order: a click-pinned token, then the hovered token, then keeping the open
    /// popover alive while the cursor is over the panel itself. A primary click on a token
    /// toggles the pin; Escape clears it.
    pub fn update_popover(
        &mut self,
        hover: Option<usize>,
        cursor: Option<egui::Pos2>,
        clicked: bool,
        escape: bool,
        tokens: &[Token],
    ) {
        if escape {
            self.pinned_token = None;
        }
        if clicked {
            match hover {
                Some(index) => {
                    self.pinned_token = (self.pinned_token != Some(index)).then_some(index);
                }
                None => {
                    let over_panel = cursor
                        .zip(self.popover.as_ref().and_then(|popover| popover.rect))
                        .is_some_and(|(pos, rect)| rect.contains(pos));
                    if !over_panel {
                        self.pinned_token = None;
                    }
                }
            }
        }

        let over_panel = cursor
            .zip(self.popover.as_ref().and_then(|popover| popover.rect))
            .is_some_and(|(pos, rect)| rect.contains(pos));
        let selected = self.pinned_token.or(hover).or_else(|| {
            over_panel
                .then(|| self.popover.as_ref().map(|p| p.token_index))
                .flatten()
        });

        match selected {
            Some(index) if self.popover.as_ref().map(|p| p.token_index) != Some(index) => {
                let surface = tokens
                    .get(index)
                    .map(|token| token.surface.clone())
                    .unwrap_or_default();
                let data = self.lookup_popover(&surface);
                self.popover = Some(PopoverState {
                    token_index: index,
                    data,
                    rect: None,
                });
            }
            Some(_) => {}
            None => self.popover = None,
        }
    }

    /// Poll the capture thread and surface state changes (started / retrying) as one-off toasts.
    pub fn update_audio_state(&mut self) {
        let Some(audio) = &self.audio else {
            return;
        };
        let status = audio.status();
        let key = format!(
            "{}|{}|{}",
            status.device, status.sample_rate, status.channels
        );
        match &status.last_error {
            Some(err) => {
                if self.audio_last_error.as_deref() != Some(err.as_str()) {
                    self.audio_last_error = Some(err.clone());
                    self.toasts
                        .push(format!("audio capture retrying: {err}"), true);
                }
            }
            None => {
                if !status.device.is_empty() && self.audio_active_key.as_deref() != Some(&key) {
                    self.audio_active_key = Some(key);
                    tracing::info!(
                        device = %status.device,
                        rate = status.sample_rate,
                        "audio capture active"
                    );
                    self.toasts.push(
                        format!(
                            "audio capture: {} @ {} Hz",
                            status.device, status.sample_rate
                        ),
                        false,
                    );
                }
                self.audio_last_error = None;
            }
        }
    }

    /// Advance timing for this frame: pick the active cue from `clock + offset`.
    pub fn update_timing(&mut self) {
        if self.clock.is_playing() {
            self.ever_started = true;
        }
        let effective = clock::effective_time(self.clock.as_ref(), self.user_offset_ms);
        self.active_cue = match &self.track {
            Some(track) => track.active_at(effective),
            None => None,
        };
    }

    /// Whether the overlay should show the "press the clock hotkey when the video starts"
    /// hint: a manual clock that has never advanced, with subtitles loaded.
    pub fn waiting_for_start(&self) -> bool {
        !self.ever_started
            && self.track.is_some()
            && self.config.clock.source == ClockSource::Manual
    }

    /// Switch the clock source live (settings card): shuts the old clock down, builds
    /// the new one sharing the manual handle, and resets the wait state so the
    /// overlay's start hint applies to the fresh clock.
    pub fn switch_clock_source(&mut self, source: ClockSource) {
        if self.config.clock.source == source {
            return;
        }
        self.config.clock.source = source;
        self.clock.shutdown();
        let (clock, fallback) = crate::clock::build_clock(
            &self.config.clock,
            self.manual.clone(),
            self.events_tx.clone(),
            Arc::clone(&self.repaint),
        );
        self.clock = clock;
        self.ever_started = false;
        self.active_cue = None;
        self.last_sync_state = None;
        if let Some(err) = fallback {
            self.toasts.push(MlError::Clock(err).to_string(), true);
        } else {
            self.toasts
                .push(format!("clock source: {}", source.label()), false);
        }
    }

    /// Persist the live config to disk (settings card). The app otherwise never writes;
    /// a missing path (pure-default run) is reported, not failed.
    pub fn save_config(&mut self) {
        match &self.config_path {
            Some(path) => match self.config.save(path) {
                Ok(()) => {
                    self.toasts.push(format!("saved {}", path.display()), false);
                }
                Err(err) => {
                    self.toasts.push(format!("save failed: {err}"), true);
                }
            },
            None => {
                self.toasts.push(
                    "no config file this run (defaults only) — nothing saved".to_owned(),
                    true,
                );
            }
        }
    }

    /// Cache key + prompt parts for explaining the open popover: `(key, sentence, focus)`.
    /// The sentence is the active cue's full text (the token alone loses context).
    pub fn explanation_key(&self) -> Option<(String, String, String)> {
        let popover = self.popover.as_ref()?;
        let term = popover.data.term.clone();
        let reading = popover.data.reading.clone();
        let sentence = match (&self.track, self.active_cue) {
            (Some(track), Some(idx)) => track.cue(idx).map(|c| c.text.clone()),
            _ => None,
        }
        .unwrap_or_else(|| term.clone());
        let focus = if reading.is_empty() {
            term.clone()
        } else {
            format!("{term} ({reading})")
        };
        Some((format!("{sentence}｜{term}"), sentence, focus))
    }

    /// Ask the local LLM to explain the open popover, off the UI thread. Cached answers
    /// are reused; at most one request is in flight.
    pub fn request_explanation(&mut self) {
        let Some((key, sentence, focus)) = self.explanation_key() else {
            return;
        };
        if self.explanations.contains_key(&key) || self.explaining.is_some() {
            return;
        }
        let Some(req) =
            crate::explain::ExplainRequest::from_config(&self.config.explain, sentence, focus)
        else {
            self.toasts.push(
                "set [explain] endpoint in config.toml to enable explanations".to_owned(),
                true,
            );
            return;
        };
        self.explaining = Some(key.clone());
        let tx = self.events_tx.clone();
        std::thread::spawn(move || {
            let result = crate::explain::fetch(&req).map_err(|err| err.to_string());
            let _ = tx.send(CoreEvent::Explanation { key, result });
        });
    }

    /// Handle local (overlay-focused) keys for M2: `[`/`]` offset, `Space`
    /// play/pause, arrows seek — the global variants arrive in M7 via `global-hotkey`.
    pub fn handle_local_keys(&mut self, ui: &egui::Ui) {
        let step = self.config.subtitle.offset_step_ms;
        let actions = ui.input(|i| {
            if !i.focused {
                return None;
            }
            Some((
                i.key_pressed(egui::Key::OpenBracket),
                i.key_pressed(egui::Key::CloseBracket),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Space),
            ))
        });
        let Some((back, fwd, seek_back, seek_fwd, toggle)) = actions else {
            return;
        };
        if back {
            self.adjust_offset(-step);
        }
        if fwd {
            self.adjust_offset(step);
        }
        if toggle {
            self.manual.toggle_play_pause();
        }
        if seek_back {
            self.manual.seek_by(Duration::from_secs(5), false);
        }
        if seek_fwd {
            self.manual.seek_by(Duration::from_secs(5), true);
        }
    }

    /// Adjust the fine offset by `delta_ms` and show a toast.
    pub fn adjust_offset(&mut self, delta_ms: i64) {
        self.user_offset_ms = self.user_offset_ms.saturating_add(delta_ms);
        self.config.subtitle.user_offset_ms = self.user_offset_ms;
        self.toasts
            .push(format!("offset {:+} ms", self.user_offset_ms), false);
    }

    // -------------------------------------------------------------------------------------
    // M7: global hotkey actions + mining export
    // -------------------------------------------------------------------------------------

    /// Drain global hotkey presses and apply them. Called every frame; a pressed hotkey is
    /// translated by [`AppAction`] into a real action below (never a silent no-op).
    pub fn poll_hotkeys(&mut self) {
        let Some(actions) = self.hotkeys.as_mut().map(GlobalHotkeys::poll) else {
            return;
        };
        for action in actions {
            tracing::debug!(?action, "global hotkey pressed");
            self.apply_action(action);
        }
    }

    fn apply_action(&mut self, action: AppAction) {
        self.last_action = Some((Instant::now(), action.label()));
        match action {
            AppAction::ExportCard => {
                self.export_current_card();
            }
            AppAction::OffsetBack => {
                let step = self.config.subtitle.offset_step_ms;
                self.adjust_offset(-step);
            }
            AppAction::OffsetForward => {
                let step = self.config.subtitle.offset_step_ms;
                self.adjust_offset(step);
            }
            AppAction::ToggleLock => {
                self.locked = !self.locked;
                self.toasts.push(
                    format!(
                        "overlay {}",
                        if self.locked { "locked" } else { "unlocked" }
                    ),
                    false,
                );
            }
            AppAction::ToggleVisibility => {
                self.visible = !self.visible;
                self.toasts.push(
                    format!("overlay {}", if self.visible { "shown" } else { "hidden" }),
                    false,
                );
            }
            AppAction::OpenSubtitle => self.open_subtitle_dialog(),
            AppAction::ClockStartPause => {
                self.manual.toggle_play_pause();
            }
            AppAction::ClockSeekBack => {
                self.manual.seek_by(Duration::from_secs(5), false);
            }
            AppAction::ClockSeekForward => {
                self.manual.seek_by(Duration::from_secs(5), true);
            }
            AppAction::ToggleEditMode => {
                self.edit_mode = !self.edit_mode;
                self.toasts.push(
                    format!(
                        "edit mode {} (editing UI arrives with the settings panel)",
                        if self.edit_mode { "on" } else { "off" }
                    ),
                    false,
                );
            }
            AppAction::ToggleStatus => {
                self.show_status = !self.show_status;
                self.toasts.push(
                    format!(
                        "status strip {}",
                        if self.show_status { "shown" } else { "hidden" }
                    ),
                    false,
                );
            }
            AppAction::ToggleDashboard => {
                self.config.window.dashboard_open = !self.config.window.dashboard_open;
                self.toasts.push(
                    format!(
                        "dashboard {}",
                        if self.config.window.dashboard_open {
                            "opened"
                        } else {
                            "closed"
                        }
                    ),
                    false,
                );
            }
        }
    }

    /// Recompute the per-token dictionary rows for the dashboard when the active cue text
    /// changed (LRU-cached lookups; runs only while the dashboard is open).
    pub fn refresh_dashboard_tokens(&mut self) {
        let text = match self
            .active_cue
            .and_then(|index| self.track.as_ref().and_then(|track| track.cue(index)))
        {
            Some(cue) => cue.text.clone(),
            None => String::new(),
        };
        if self
            .dashboard_tokens
            .as_ref()
            .is_some_and(|(cached, _)| *cached == text)
        {
            return;
        }
        let tokens = self.tokens_cached(&text);
        let mut rows = Vec::with_capacity(tokens.len());
        for token in tokens.iter() {
            let (resolved, reading) = if token.is_content_word() {
                match &mut self.dictionary {
                    Some(dictionary) => match dictionary.resolve_detailed(&token.surface) {
                        Ok(Some(resolution)) => {
                            let reading = resolution
                                .entries
                                .first()
                                .map(|entry| entry.reading.clone())
                                .unwrap_or_else(|| token.reading.clone());
                            (true, reading)
                        }
                        // Unresolved content word: dictionary miss is displayed honestly.
                        Ok(None) => (false, token.reading.clone()),
                        Err(err) => {
                            tracing::debug!(error = %err, surface = %token.surface, "dictionary miss in dashboard");
                            (false, token.reading.clone())
                        }
                    },
                    None => (false, token.reading.clone()),
                }
            } else {
                (false, token.reading.clone())
            };
            rows.push(TokenRow {
                surface: token.surface.clone(),
                reading,
                content: token.is_content_word(),
                resolved,
            });
        }
        self.dashboard_tokens = Some((text, rows));
    }

    /// File dialog for a subtitle file. Linux uses the XDG desktop portal (`rfd` with the
    /// `xdg-portal` backend — no GTK dependency, matching the AGENTS.md Wayland story);
    /// other platforms fall back to drag-and-drop onto the overlay.
    fn open_subtitle_dialog(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let picked = rfd::FileDialog::new()
                .add_filter("Subtitles", &["srt", "vtt", "ass", "ssa", "sub"])
                .pick_file();
            match picked {
                Some(path) => {
                    let _ = self.loader_tx.send(UiCommand::LoadSubtitles(path));
                }
                None => {
                    self.toasts
                        .push("open subtitle cancelled".to_owned(), false);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.toasts.push(
                "file dialog is only wired for Linux (XDG portal); drag the subtitle file onto the overlay instead".to_owned(),
                true,
            );
        }
    }

    /// Gather the current cue into an [`ExportSource`] (UI thread — [`Dictionary`] is not
    /// `Sync`) and hand it to the export worker. Returns `false` with a toast when nothing
    /// is mineable or the worker is busy.
    pub fn export_current_card(&mut self) -> bool {
        // Copy the pieces we need out of the track first: the borrow on `self.track` must
        // end before we call `&mut self` methods below.
        let Some(index) = self.active_cue else {
            self.toasts.push(
                "nothing to export: no active cue (pause/seek into a line)".to_owned(),
                true,
            );
            return false;
        };
        let Some((text, cue_start, cue_end, source_name)) = (|| {
            let track = self.track.as_ref()?;
            let cue = track.cue(index)?;
            let source_name = track
                .path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "subtitle".to_owned());
            Some((cue.text.clone(), cue.start, cue.end, source_name))
        })() else {
            self.toasts.push(
                "nothing to export: no subtitle track loaded".to_owned(),
                true,
            );
            return false;
        };

        let tokens = self.tokens_cached(&text);
        // Prefer the hovered/pinned token; fall back to the first content word of the cue.
        let surface = self
            .hover_token
            .or(self.pinned_token)
            .and_then(|index| tokens.get(index))
            .map(|token| token.surface.clone())
            .or_else(|| {
                tokens
                    .iter()
                    .find(|token| token.is_content_word())
                    .map(|token| token.surface.clone())
            });
        let Some(surface) = surface else {
            self.toasts.push(
                "nothing to export: hover a token in the line first".to_owned(),
                true,
            );
            return false;
        };

        let resolution = match &mut self.dictionary {
            Some(dictionary) => match dictionary.resolve_detailed(&surface) {
                Ok(resolution) => resolution,
                Err(err) => {
                    tracing::warn!(error = %err, surface, "dictionary lookup failed during export");
                    None
                }
            },
            None => None,
        };
        let card = mine_card(
            &surface,
            resolution.as_ref(),
            &text,
            "",
            cue_start.as_secs_f64(),
            &source_name,
        );

        // Audio window around the cue, only when a ring exists (the worker skips media it
        // cannot produce — e.g. the buffer ran past the line — with a note in the result).
        let ring = self.audio.as_ref().and_then(|audio| audio.ring());
        let audio_window = ring.as_ref().map(|_| {
            let pad = 0.4;
            let start = (cue_start.as_secs_f64() - pad).max(0.0);
            let end = (cue_end.as_secs_f64() + pad).max(start + 1.5);
            (start, end)
        });

        let source = ExportSource {
            card,
            anki: self.config.anki.clone(),
            capture: self.config.capture.clone(),
            queue_dir: self.queue_dir.clone(),
            ring,
            audio_window,
            nonce: rand::random(),
        };
        let accepted = self
            .export_worker
            .as_ref()
            .is_some_and(|worker| worker.submit(source));
        if accepted {
            self.toasts.push(format!("exporting '{surface}'…"), false);
            true
        } else {
            self.toasts.push(
                "export worker busy — try again in a moment".to_owned(),
                true,
            );
            false
        }
    }

    /// Ask every worker to stop and join it. Safe to call more than once.
    pub fn shutdown_workers(&mut self) {
        let _ = self.loader_tx.send(UiCommand::Shutdown);
        self.clock.shutdown();
        self.audio = None; // drop joins the capture thread
        if let Some(mut worker) = self.export_worker.take() {
            worker.stop();
            worker.join();
        }
        for hook in std::mem::take(&mut self.shutdown_hooks) {
            hook();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.drain_config_issues();
        self.poll_events();
        self.update_audio_state();

        // Accept subtitle files dropped onto the overlay.
        let dropped = ui.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            let path = file.path();
            if !path.as_os_str().is_empty() {
                let _ = self
                    .loader_tx
                    .send(UiCommand::LoadSubtitles(path.to_path_buf()));
            }
        }

        self.handle_local_keys(ui);
        self.poll_hotkeys();
        self.update_timing();
        self.update_cursor(frame, ui.ctx());

        // Surface clock sync-state transitions as toasts (e.g. mpv connected/lost).
        let sync = self.clock.sync_state();
        if self.last_sync_state.as_ref() != Some(&sync) {
            if let crate::clock::SyncState::Stale(reason) = &sync {
                self.toasts.push(reason.clone(), true);
            }
            self.last_sync_state = Some(sync);
        }

        crate::gui::overlay::show(self, ui);

        // Control-room dashboard: a normal opaque window beside the overlay. Immediate
        // viewports open while we keep calling this and close when we stop (i.e. when the
        // user closes it or toggles it off), so the state lives in `config.window`.
        if self.config.window.dashboard_open {
            crate::gui::dashboard::show(self, ui.ctx());
        }

        // eframe 0.36 ignores `ViewportBuilder::with_window_level`, so re-assert the EWMH
        // `_NET_WM_STATE_ABOVE` at a low cadence (self-heals if the WM drops it). Cheap:
        // one X tree walk per check; a ClientMessage is only sent when the state differs.
        if self.last_topmost_check.elapsed() >= Duration::from_millis(750) {
            self.last_topmost_check = Instant::now();
            if let Err(err) = self.pointer.set_always_on_top(
                crate::platform::OVERLAY_TITLE,
                self.config.window.always_on_top,
            ) {
                if !self.topmost_error_logged {
                    self.topmost_error_logged = true;
                    tracing::warn!(error = %err, "always-on-top enforcement unavailable");
                    self.toasts
                        .push(format!("always-on-top unavailable: {err}"), true);
                }
            }
            // One taskbar icon instead of two (user review 2026-10-10). No-op while the
            // dashboard is closed; errors stay silent (the overlay hint is the one that
            // matters, and it already reports).
            if let Err(err) = self
                .pointer
                .set_skip_taskbar(crate::platform::DASHBOARD_TITLE)
            {
                tracing::debug!(error = %err, "skip-taskbar unavailable");
            }
        }

        // First-run placement: the default rect sentinel (negative x/y) means
        // "bottom-center of the primary screen" — real subtitles live at the bottom,
        // above the video controls instead of over the browser tabs. Uses the X11 root
        // geometry (physical pixels / ppp), never the viewport rect (window-local).
        if !self.placed {
            self.placed = true;
            let size = [self.config.window.rect[2], self.config.window.rect[3]];
            match self.pointer.primary_screen_size() {
                Ok((w, h)) => {
                    let ppp = ui.ctx().pixels_per_point();
                    let screen = egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::Vec2::new(w as f32 / ppp, h as f32 / ppp),
                    );
                    let rect = bottom_center_rect(screen, size);
                    self.config.window.rect = rect;
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::Pos2::new(
                            rect[0], rect[1],
                        )));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "auto bottom-center placement unavailable");
                }
            }
        }

        // Fullscreen-follow: while the video window is fullscreen, expand the overlay to
        // fullscreen too (KWin keeps fullscreen windows above always-on-top ones, so this
        // is the only way to keep subtitles visible over a fullscreen player). Restores
        // the saved rect when the player leaves fullscreen.
        if self.config.window.follow_fullscreen
            && self.last_fullscreen_check.elapsed() >= Duration::from_millis(750)
        {
            self.last_fullscreen_check = Instant::now();
            match self.pointer.is_any_fullscreen() {
                Ok(true) => {
                    if !self.following_fullscreen {
                        self.following_fullscreen = true;
                        self.saved_window_rect = Some(self.config.window.rect);
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    }
                }
                Ok(false) => {
                    if self.following_fullscreen {
                        self.following_fullscreen = false;
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                        if let Some(rect) = self.saved_window_rect.take() {
                            if rect[0] >= 0.0 && rect[1] >= 0.0 {
                                ui.ctx()
                                    .send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                                        egui::Pos2::new(rect[0], rect[1]),
                                    ));
                            }
                        }
                    }
                }
                Err(err) => {
                    if !self.fullscreen_logged {
                        self.fullscreen_logged = true;
                        tracing::warn!(error = %err, "fullscreen-follow unavailable");
                        self.toasts
                            .push(format!("fullscreen-follow unavailable: {err}"), true);
                    }
                }
            }
        }

        // Enable/disable click-through from the interactive regions the overlay just
        // computed. Deferred viewport commands apply after this frame.
        self.apply_passthrough(ui.ctx());

        // The global-cursor hover mechanism needs a steady tick even when idle (the window
        // receives no mouse events while click-through), so poll at ~30 Hz; the clock's own
        // tick keeps cues advancing.
        ui.ctx().request_repaint_after(Duration::from_millis(33));
        if self.clock.is_playing() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }

    /// Fully transparent clear color — the whole point of the overlay window.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown_workers();
    }
}

/// Bottom-center placement for the overlay: centered horizontally, sitting just above the
/// bottom edge so it clears panels/taskbars. Pure and testable.
pub fn bottom_center_rect(screen: egui::Rect, size: [f32; 2]) -> [f32; 4] {
    const BOTTOM_GAP: f32 = 48.0;
    let x = (screen.center().x - size[0] / 2.0).max(screen.left());
    let y = (screen.bottom() - BOTTOM_GAP - size[1]).max(screen.top());
    [x, y, size[0], size[1]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_clock_source_rebuilds_and_resets_wait_state() {
        use crate::config::ClockSource;

        let mut app = App::new(
            Config::default(),
            None,
            Vec::new(),
            crate::gui::fonts::FontSource::Embedded,
            egui::Context::default(),
        );
        assert_eq!(app.config.clock.source, ClockSource::Manual);
        // Same source: no-op, no toast.
        app.switch_clock_source(ClockSource::Manual);
        assert_eq!(app.config.clock.source, ClockSource::Manual);

        // Live switch to mpv: clock rebuilt (IPC thread spawned in background), a toast
        // announces the change.
        app.switch_clock_source(ClockSource::MpvIpc);
        assert_eq!(app.config.clock.source, ClockSource::MpvIpc);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.message.contains("clock source: mpv")),
            "expected an announce toast"
        );

        // Switch back: old clock shuts down, ever_started resets so the start hint
        // applies to the fresh clock.
        app.switch_clock_source(ClockSource::Manual);
        assert_eq!(app.config.clock.source, ClockSource::Manual);
        assert!(!app.ever_started);
    }

    #[test]
    fn save_config_persists_live_changes_to_disk() {
        use crate::config::ClockSource;

        let dir = std::env::temp_dir().join(format!("ml-save-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut app = App::new(
            Config::default(),
            Some(path.clone()),
            Vec::new(),
            crate::gui::fonts::FontSource::Embedded,
            egui::Context::default(),
        );
        app.config.clock.source = ClockSource::Mpris;
        app.config.subtitle.offset_step_ms = 250;
        app.save_config();
        assert!(path.exists(), "config file should be written");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("source = \"mpris\""), "{text}");
        let parsed: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.clock.source, ClockSource::Mpris);
        assert_eq!(parsed.subtitle.offset_step_ms, 250);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bottom_center_rect_is_centered_above_the_bottom_edge() {
        let screen =
            egui::Rect::from_min_size(egui::Pos2::new(0.0, 0.0), egui::Vec2::new(1920.0, 1080.0));
        let rect = bottom_center_rect(screen, [1100.0, 320.0]);
        assert_eq!(rect[0], 410.0); // (1920 - 1100) / 2
        assert_eq!(rect[1], 1080.0 - 48.0 - 320.0);
        assert_eq!([rect[2], rect[3]], [1100.0, 320.0]);
    }

    #[test]
    fn bottom_center_rect_small_screen_is_clamped() {
        let screen =
            egui::Rect::from_min_size(egui::Pos2::new(0.0, 0.0), egui::Vec2::new(400.0, 300.0));
        // Overlay wider than the screen: x clamps to the left edge, y to the top.
        let rect = bottom_center_rect(screen, [1100.0, 320.0]);
        assert_eq!(rect[0], 0.0);
        assert_eq!(rect[1], 0.0);
    }

    #[test]
    fn waiting_hint_shows_until_the_manual_clock_first_advances() {
        use std::path::{Path, PathBuf};

        use crate::config::ClockSource;
        use crate::subs::{parser, Format, SubtitleTrack};

        let mut app = App::new(
            Config::default(),
            None,
            Vec::new(),
            crate::gui::fonts::FontSource::Embedded,
            egui::Context::default(),
        );
        // No track loaded: nothing to wait for, no hint.
        assert!(!app.waiting_for_start());
        assert_eq!(app.config.clock.source, ClockSource::Manual);

        let fixture = Path::new("tests/fixtures/simple.srt");
        let text = std::fs::read_to_string(fixture).unwrap();
        let (cues, skipped) = parser::parse(Format::Srt, &text, fixture);
        app.track = Some(
            SubtitleTrack::new(
                cues,
                PathBuf::from(fixture),
                Format::Srt,
                "UTF-8".to_owned(),
                skipped,
            )
            .unwrap(),
        );

        // Manual clock paused at zero: the hint shows.
        app.update_timing();
        assert!(app.waiting_for_start());

        // First play clears it for good — pausing again must not bring it back.
        app.manual.set_playing(true);
        app.update_timing();
        assert!(!app.waiting_for_start());
        app.manual.set_playing(false);
        app.update_timing();
        assert!(!app.waiting_for_start());
    }
}
