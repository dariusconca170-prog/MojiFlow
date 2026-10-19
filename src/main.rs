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

const USAGE: &str = "\
medialingual-native [--subtitle <file>]

  --subtitle, -s <file>   load this subtitle file at startup (srt/vtt/ass/ssa/sub)
  --help, -h              print this help and exit

Without `--subtitle` the overlay starts empty: load a file with Ctrl+Alt+O, by dragging it
onto the overlay, or from the dashboard.";

/// Parse the command line: `Ok(Some(path))` to preload a subtitle file, `Ok(None)` for none.
/// `Err(message)` is a usage error (unknown flag, missing argument) the caller reports.
/// Pure so the flag handling is unit-testable without launching a window.
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Option<PathBuf>, String> {
    let mut args = args.into_iter();
    let mut preload = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--subtitle" | "-s" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("'{arg}' needs a file path"))?;
                if value.trim().is_empty() {
                    return Err(format!("'{arg}' needs a file path"));
                }
                preload = Some(PathBuf::from(value));
            }
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unrecognised argument '{other}'")),
        }
    }
    Ok(preload)
}

fn main() -> anyhow::Result<()> {
    let preload = match parse_args(std::env::args().skip(1)) {
        Ok(preload) => preload,
        Err(message) => {
            // An empty message is the explicit `--help` request, which is not an error.
            if message.is_empty() {
                println!("{USAGE}");
                return Ok(());
            }
            eprintln!("medialingual: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

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
        .with_title(medialingual_native::platform::OVERLAY_TITLE)
        .with_transparent(true)
        // Note: eframe 0.36 drops window-level hints entirely (no WindowLevel handling in
        // its winit integration), so this is intent only — the real `_NET_WM_STATE_ABOVE`
        // is asserted at runtime by `App` (see `GlobalPointer::set_always_on_top`).
        .with_window_level(if config.window.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        })
        .with_decorations(config.window.show_decorations)
        .with_taskbar(false)
        // Start click-through; `App::apply_passthrough` re-enables input only while the
        // cursor is over an interactive region (see AGENTS.md).
        .with_mouse_passthrough(true)
        // X11/KWin fullscreen stacking: a *managed* always-on-top window loses to a
        // fullscreen player, and the runtime EWMH enforcer can't beat that either. An
        // override-redirect window is unmanaged, so the WM never restacks it under the
        // video; `Utility` is the closest EWMH type for a persistent palette. Trade-off:
        // the WM gives this window no keyboard focus (global hotkeys remain the input
        // path) and the always-on-top EWMH enforcer becomes a no-op.
        .with_override_redirect(true)
        .with_window_type(egui::viewport::X11WindowType::Utility)
        // A negative rect component is the auto bottom-center sentinel — let the WM place
        // the window and let `App` move it on the first frame.
        .with_position([
            if config.window.rect[0] >= 0.0 {
                config.window.rect[0]
            } else {
                0.0
            },
            if config.window.rect[1] >= 0.0 {
                config.window.rect[1]
            } else {
                0.0
            },
        ])
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
            if let Some(path) = preload {
                // The loader runs off the UI thread, so this only queues the work.
                if !app.load_subtitle_file(path.clone()) {
                    tracing::error!(path = %path.display(), "subtitle loader unavailable");
                } else {
                    tracing::info!(path = %path.display(), "preloading subtitle file");
                }
            }
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    );
    // eframe::Error is not std::error::Error on all versions, so format it explicitly.
    result.map_err(|err| anyhow::anyhow!("eframe failed to start: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn subtitle_flag_accepts_both_spellings() {
        assert_eq!(
            parse_args(args(&["--subtitle", "ep.srt"])).expect("parses"),
            Some(PathBuf::from("ep.srt"))
        );
        assert_eq!(
            parse_args(args(&["-s", "/tmp/ep 1.ass"])).expect("parses"),
            Some(PathBuf::from("/tmp/ep 1.ass"))
        );
        assert_eq!(parse_args(args(&[])).expect("parses"), None);
    }

    #[test]
    fn bad_arguments_are_reported_not_ignored() {
        assert!(parse_args(args(&["--subtitle"]))
            .expect_err("missing path")
            .contains("needs a file path"));
        assert!(parse_args(args(&["--nope"]))
            .expect_err("unknown flag")
            .contains("unrecognised argument '--nope'"));
        // `--help` is a request, not a failure: an empty message.
        assert!(parse_args(args(&["--help"])).is_err_and(|message| message.is_empty()));
    }
}
