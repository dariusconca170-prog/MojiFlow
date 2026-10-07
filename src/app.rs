//! Application state machine and the UI ↔ worker message bus.
//!
//! `UiCommand` flows UI → workers, `CoreEvent` flows workers → UI through bounded channels.
//! Workers hold `egui::Context` handles and call `request_repaint()` when they post an event.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::MlError;

/// Commands sent from the UI thread to worker threads/tasks.
#[derive(Debug)]
pub enum UiCommand {
    /// Load a subtitle track from a path (drag-and-drop or file dialog).
    LoadSubtitles(std::path::PathBuf),
    /// Set the playback clock source at runtime.
    SetClockSource(crate::config::ClockSource),
    /// Adjust the user subtitle offset by `delta_ms`.
    AdjustOffset { delta_ms: i64 },
    /// Export the active cue as an Anki note (audio/image capture included).
    ExportCard,
    /// Stop every worker and release resources.
    Shutdown,
}

/// Events posted by workers to the UI thread.
#[derive(Debug)]
pub enum CoreEvent {
    /// A subsystem reported a human-readable status change (status strip).
    Status { component: &'static str, text: String },
    /// A non-fatal error to surface as a toast.
    Warning(MlError),
    /// A fatal-but-recoverable error; the app keeps running, the worker stopped.
    WorkerFailed { worker: &'static str, error: MlError },
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

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
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
    /// Milestone-1 demo content until subtitle tracks land in M2.
    pub demo_text: String,
}

impl App {
    pub fn new(
        config: Config,
        config_path: Option<std::path::PathBuf>,
        config_issues: Vec<MlError>,
        font_source: crate::gui::fonts::FontSource,
    ) -> Self {
        Self {
            config,
            config_path,
            config_issues,
            toasts: ToastQueue::default(),
            font_source,
            demo_text: "日本語の文をマイニングしよう。漢字・かな・々・ー・〜 すべて表示されます。"
                .to_owned(),
        }
    }

    /// Surface config load issues exactly once, on the first frame.
    pub fn drain_config_issues(&mut self) {
        for issue in std::mem::take(&mut self.config_issues) {
            self.toasts.push(issue.to_string(), true);
        }
    }

    pub fn now(&self) -> Instant {
        Instant::now()
    }
}
