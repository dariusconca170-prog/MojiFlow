//! MediaLingual-Native — entry point: logging, config load, eframe launch.
//!
//! Everything heavy runs off the UI thread (see AGENTS.md); this file only wires up
//! logging, configuration and the window.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::writer::MakeWriterExt as _;

use medialingual_native::app::App;
use medialingual_native::config::Config;
use medialingual_native::error::MlError;
use medialingual_native::gui::fonts::FontSource;

/// Cloneable writer that appends to the log file, recovering from lock poisoning rather
/// than panicking (logging must never abort the app).
#[derive(Clone)]
struct SharedLogFile(Arc<Mutex<std::fs::File>>);

struct SharedLogFileWriter<'a>(std::sync::MutexGuard<'a, std::fs::File>);

impl std::io::Write for SharedLogFileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogFile {
    type Writer = SharedLogFileWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        SharedLogFileWriter(
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// Set up tracing: everything to a log file next to `config.toml`, warnings+ also to stderr.
fn init_tracing(log_path: Option<PathBuf>) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_writer = log_path.and_then(|path| {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::File::options()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => Some(SharedLogFile(Arc::new(Mutex::new(file)))),
            Err(err) => {
                eprintln!(
                    "medialingual: cannot open log file {}: {err}",
                    path.display()
                );
                None
            }
        }
    });

    match file_writer {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file.and(std::io::stderr.with_max_level(tracing::Level::WARN)))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init(),
    }
    Ok(())
}

fn log_session_type() {
    let session = medialingual_native::platform::detect();
    tracing::info!(
        session = %session.label(),
        os = std::env::consts::OS,
        x11 = std::env::var("DISPLAY").is_ok(),
        "session detected"
    );
    medialingual_native::platform::log_session(session);
}

fn main() -> anyhow::Result<()> {
    let config_path = Config::default_path();
    let log_path = config_path
        .as_ref()
        .map(|p| p.with_file_name("medialingual.log"));
    init_tracing(log_path)?;
    log_session_type();

    let (config, issues, config_path) = match config_path {
        Some(path) => {
            let loaded = Config::load(&path);
            for issue in &loaded.issues {
                tracing::warn!("config: {issue}");
            }
            tracing::info!(path = %path.display(), "config loaded");
            (loaded.config, loaded.issues, Some(path))
        }
        None => {
            tracing::warn!("no config directory available; using built-in defaults");
            (Config::default(), Vec::new(), None)
        }
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("MediaLingual")
        .with_transparent(true)
        .with_always_on_top()
        .with_decorations(config.window.show_decorations)
        .with_taskbar(false)
        // Start click-through; `App::apply_passthrough` re-enables input only while the
        // cursor is over an interactive region (see AGENTS.md).
        .with_mouse_passthrough(true)
        .with_position([config.window.rect[0], config.window.rect[1]])
        .with_inner_size([config.window.rect[2], config.window.rect[3]]);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let issues: Vec<MlError> = issues.into_iter().map(MlError::Config).collect();

    let result = eframe::run_native(
        "MediaLingual",
        options,
        Box::new(move |cc| {
            let mut issues = issues;
            // Font must be installed before the first frame or Japanese renders as tofu.
            let font_source = match medialingual_native::gui::fonts::install_cjk_fonts(&cc.egui_ctx)
            {
                Ok(source) => {
                    tracing::info!(source = %source, "CJK font installed");
                    source
                }
                Err(err) => {
                    tracing::error!(error = %err, "CJK font install failed");
                    let message = err.to_string();
                    issues.push(MlError::Font(err));
                    FontSource::Failed(message)
                }
            };
            let app = App::new(
                config,
                config_path,
                issues,
                font_source,
                cc.egui_ctx.clone(),
            );
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    );
    // eframe::Error is not std::error::Error on all versions, so format it explicitly.
    result.map_err(|err| anyhow::anyhow!("eframe failed to start: {err}"))
}
