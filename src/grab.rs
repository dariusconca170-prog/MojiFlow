//! "Grab video" (M8): download a stream via the user-installed `yt-dlp` binary.
//!
//! Site-specific extraction is yt-dlp's job (thousands of sites, maintained daily) — the
//! app never reimplements it. This module only builds the argument list, runs it off the UI
//! thread (polling `try_wait` so a quit can cancel mid-download), and parses the final
//! output path from yt-dlp's `--print after_move:filepath`. The resulting file can then be
//! opened in mpv (with the JSON-IPC socket), after which the overlay follows playback
//! automatically via `MpvIpcClock`.
//!
//! Honest limits, surfaced as typed [`GrabError`]s: DRM'd streams cannot be downloaded by
//! any tool, and merging `bv*+ba` needs the `ffmpeg` binary on PATH.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::GrabConfig;

/// Everything the yt-dlp run needs besides the process handle.
#[derive(Debug, Clone)]
pub struct GrabSpec {
    pub url: String,
    pub out_dir: PathBuf,
    pub format: String,
    pub sub_langs: String,
    pub convert_subs: bool,
}

impl GrabSpec {
    /// Derive the spec from the persisted config.
    pub fn from_config(config: &GrabConfig, url: String) -> Self {
        Self {
            url,
            out_dir: expand_tilde(&config.output_dir),
            format: config.format.clone(),
            sub_langs: config.sub_langs.clone(),
            convert_subs: config.convert_subs,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GrabError {
    #[error("yt-dlp not found on PATH (install with: sudo apt install yt-dlp)")]
    MissingBinary,
    #[error("yt-dlp exited with status {code}: {stderr}")]
    Failed { code: i32, stderr: String },
    #[error("yt-dlp produced no media file: {stderr}")]
    NoOutput { stderr: String },
    #[error("cannot run yt-dlp: {0}")]
    Run(String),
}

/// Handle shared between the UI and the running grab thread: cancellation flag plus a
/// "busy" flag so the dashboard can disable the Grab button while a download is running.
#[derive(Debug)]
pub struct GrabShared {
    pub cancel: AtomicBool,
    pub running: AtomicBool,
}

impl Default for GrabShared {
    fn default() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            running: AtomicBool::new(false),
        }
    }
}

/// Expand a leading `~` to `$HOME` (yt-dlp would reject `~/…` in `-o`).
fn expand_tilde(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// Build the `yt-dlp` argument list. `program` is injectable for tests.
fn build_command(program: &str, spec: &GrabSpec) -> Command {
    let mut cmd = Command::new(program);
    // `--print after_move:filepath` makes yt-dlp print the final media path on stdout as
    // its last action — that is what `parse_destination` reads.
    cmd.arg("--no-playlist")
        .arg("--no-progress")
        .arg("-f")
        .arg(&spec.format)
        .arg("-o")
        .arg(spec.out_dir.join("%(title)s.%(ext)s"))
        .arg("--print")
        .arg("after_move:filepath")
        .arg(&spec.url);
    if !spec.sub_langs.trim().is_empty() {
        cmd.arg("--write-subs")
            .arg("--sub-langs")
            .arg(&spec.sub_langs);
        if spec.convert_subs {
            cmd.arg("--convert-subs").arg("srt");
        }
    }
    cmd
}

/// Run yt-dlp to completion (or cancellation). Polls `try_wait` so the UI can kill the
/// child mid-download by raising `shared.cancel`. Returns the downloaded media path.
pub fn run(program: &str, spec: &GrabSpec, shared: &GrabShared) -> Result<PathBuf, GrabError> {
    let mut child = build_command(program, spec)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                GrabError::MissingBinary
            } else {
                GrabError::Run(err.to_string())
            }
        })?;

    shared.running.store(true, Ordering::SeqCst);
    let status = {
        loop {
            if shared.cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(err) => {
                    let _ = child.kill();
                    return Err(GrabError::Run(err.to_string()));
                }
            }
        }
    };
    shared.running.store(false, Ordering::SeqCst);

    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());

    if !status.success() {
        return Err(GrabError::Failed {
            code: status.code().unwrap_or(-1),
            stderr: concise(&stderr),
        });
    }
    parse_destination(&stdout).ok_or_else(|| GrabError::NoOutput {
        stderr: concise(&stderr),
    })
}

fn read_all(mut stream: Option<impl Read>) -> String {
    let mut buf = String::new();
    if let Some(stream) = stream.as_mut() {
        let _ = stream.read_to_string(&mut buf);
    }
    buf
}

/// The final `--print after_move:filepath` line, or `None` if yt-dlp printed nothing.
fn parse_destination(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

/// Last ~300 characters of stderr, for error messages that stay readable.
fn concise(stderr: &str) -> String {
    let tail_len = stderr.chars().count().min(300);
    let tail: String = stderr
        .chars()
        .skip(stderr.chars().count() - tail_len)
        .collect();
    tail
}

/// Probe the installed `yt-dlp` version (for the dashboard chip). `None` = not installed.
pub fn probe_version() -> Option<String> {
    let output = Command::new("yt-dlp").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!version.is_empty()).then_some(version)
}

/// Open the grabbed file in a detached mpv with the JSON-IPC socket so the overlay
/// follows playback (`clock.source = "mpv_ipc"`). The child is intentionally not waited
/// on: it outlives this process and is reaped by the init process.
pub fn open_in_mpv(file: &Path, mpv_socket: &str) -> Result<(), GrabError> {
    Command::new("mpv")
        .arg(format!("--input-ipc-server={mpv_socket}"))
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                GrabError::Run("mpv not found on PATH (install with: sudo apt install mpv)".into())
            } else {
                GrabError::Run(err.to_string())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(url: &str, sub_langs: &str) -> GrabSpec {
        GrabSpec {
            url: url.to_owned(),
            out_dir: PathBuf::from("/tmp/ml-out"),
            format: "bv*+ba".to_owned(),
            sub_langs: sub_langs.to_owned(),
            convert_subs: true,
        }
    }

    #[test]
    fn command_args_cover_subs_merge_and_output() {
        let cmd = build_command("yt-dlp", &spec("https://example.com/v", "ja"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--no-playlist".to_owned()));
        assert!(args.contains(&"-f".to_owned()) && args.contains(&"bv*+ba".to_owned()));
        assert!(args.contains(&"--write-subs".to_owned()));
        assert!(args.contains(&"--sub-langs".to_owned()) && args.contains(&"ja".to_owned()));
        assert!(args.contains(&"--convert-subs".to_owned()) && args.contains(&"srt".to_owned()));
        assert!(args.contains(&"--print".to_owned()));
        assert!(args.contains(&"after_move:filepath".to_owned()));
        assert!(args.contains(&"https://example.com/v".to_owned()));
        // The `-o` template lives inside the output dir.
        let output = args[args.iter().position(|a| a == "-o").unwrap() + 1].clone();
        assert!(
            output.starts_with("/tmp/ml-out/"),
            "output template: {output}"
        );
    }

    #[test]
    fn no_subs_requested_skips_subtitle_flags() {
        let cmd = build_command("yt-dlp", &spec("https://example.com/v", ""));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.contains(&"--write-subs".to_owned()));
    }

    #[test]
    fn parses_the_last_non_empty_destination_line() {
        assert_eq!(
            parse_destination("\n[download] blah\n/home/darius/videos/出撃.mp4\n"),
            Some(PathBuf::from("/home/darius/videos/出撃.mp4"))
        );
        assert_eq!(parse_destination("  \n\n"), None);
        assert_eq!(parse_destination(""), None);
    }

    #[test]
    fn missing_binary_is_a_typed_error() {
        let shared = GrabShared::default();
        let err = run(
            "/nonexistent/yt-dlp",
            &spec("https://example.com/v", ""),
            &shared,
        )
        .unwrap_err();
        assert!(matches!(err, GrabError::MissingBinary), "{err}");
    }

    /// A fake `yt-dlp` (a shell script on disk) that writes the media file and prints the
    /// destination line, end to end.
    #[cfg(unix)]
    #[test]
    fn fake_yt_dlp_runs_and_reports_the_media_path() {
        let dir = std::env::temp_dir().join(format!("ml-grab-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-yt-dlp");
        let media = dir.join("episode.mp4");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\nprintf 'fake' > '{}'\n",
                media.display(),
                media.display()
            ),
        )
        .unwrap();
        #[allow(unused_imports)]
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let shared = GrabShared::default();
        let path = run(
            script.to_str().unwrap(),
            &spec("https://example.com/v", "ja"),
            &shared,
        )
        .unwrap();
        assert_eq!(path, media);
        assert!(media.exists(), "media file should exist");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tilde_expands_to_home() {
        assert_eq!(
            expand_tilde("~/Videos"),
            PathBuf::from(std::env::var("HOME").unwrap()).join("Videos")
        );
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
