//! Application state machine and the UI ↔ worker message bus.
//!
//! `UiCommand` flows UI → workers, `CoreEvent` flows workers → UI through bounded channels.
//! Workers hold a [`RepaintHandle`] and signal it after posting an event so the UI wakes
//! up exactly when there is something new (never busy-polling).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::capture::AudioCapture;
use crate::capture::window::GlobalPointer;
use crate::clock::{self, manual::ManualClock, PlaybackClock};
use crate::config::Config;
use crate::dict::Dictionary;
use crate::error::MlError;
use crate::gui::popover::PopoverData;
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
        self.items
            .retain(|t| now.duration_since(t.created) < TOAST_TTL);
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
    /// Milestone-1 demo content, shown until a track is loaded and a cue is active.
    pub demo_text: String,

    // --- M2: subtitles + clocks ---
    pub track: Option<SubtitleTrack>,
    /// Index of the currently active cue (into `track`).
    pub active_cue: Option<usize>,
    /// User fine offset in ms (`[`/`]`), layered on top of the clock.
    pub user_offset_ms: i64,
    clock: Box<dyn PlaybackClock>,
    /// Control handle for the manual clock (shares state with `clock` when manual).
    pub manual: ManualClock,
    events_rx: crossbeam_channel::Receiver<CoreEvent>,
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

        Self {
            user_offset_ms: config.subtitle.user_offset_ms,
            config,
            config_path,
            config_issues: issues,
            toasts: ToastQueue::default(),
            font_source,
            demo_text: "日本語の文をマイニングしよう。漢字・かな・々・ー・〜 すべて表示されます。"
                .to_owned(),
            track: None,
            active_cue: None,
            clock,
            manual,
            events_rx,
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
                }
                Ok(CoreEvent::Warning(err)) => {
                    self.toasts.push(err.to_string(), true);
                }
                Ok(CoreEvent::WorkerFailed { worker, error }) => {
                    self.toasts.push(format!("{worker} stopped: {error}"), true);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// The text currently shown: active cue if any, otherwise the demo string.
    pub fn subtitle_text(&self) -> String {
        match (&self.track, self.active_cue) {
            (Some(track), Some(idx)) => track.cue(idx).map(|c| c.text.clone()).unwrap_or_default(),
            (Some(_), None) => String::new(), // gap between cues
            (None, _) => self.demo_text.clone(),
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
                self.shift_held || self.interactive_rects.iter().any(|rect| rect.contains(pos))
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
        let effective = clock::effective_time(self.clock.as_ref(), self.user_offset_ms);
        self.active_cue = match &self.track {
            Some(track) => track.active_at(effective),
            None => None,
        };
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

    /// Ask every worker to stop and join it. Safe to call more than once.
    pub fn shutdown_workers(&mut self) {
        let _ = self.loader_tx.send(UiCommand::Shutdown);
        self.clock.shutdown();
        self.audio = None; // drop joins the capture thread
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
