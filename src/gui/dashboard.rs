//! Control-room dashboard (M7): a normal opaque window that makes the overlay legible.
//!
//! The overlay is deliberately invisible (transparent, click-through), which means the
//! user can never *see* it working. This window renders the internals live: clock source
//! and sync state, the active cue with per-token dictionary hits/misses, audio ring fill,
//! the export log, and the configured hotkeys. It is a separate egui viewport opened via
//! [`egui::Context::show_viewport_immediate`], so it is a real OS window with its own
//! decorations — never click-through, never transparent.
//!
//! Transport buttons (start/pause/seek) only drive the manual clock; a live source (mpv,
//! MPRIS) is read-only this milestone — steering those arrives with the M8 settings panel.

use std::time::Duration;

use egui::{Color32, CornerRadius, Margin, RichText, Stroke, Vec2};

use crate::app::App;
use crate::clock::SyncState;
use crate::config::ClockSource;
use crate::export::format_timestamp;
use crate::hotkey::configured_actions;
use crate::platform::detect as detect_session;

/// One token of the active cue, as shown on the dashboard: is it content, and did the
/// dictionary resolve it? The green/amber chips make lookup accuracy visible per word.
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub surface: String,
    pub reading: String,
    pub content: bool,
    pub resolved: bool,
}

// --- palette (dark, indigo accent) ---
const BG: Color32 = Color32::from_rgb(13, 15, 20);
const CARD_BG: Color32 = Color32::from_rgb(18, 21, 28);
const BORDER: Color32 = Color32::from_rgb(33, 37, 47);
const TEXT: Color32 = Color32::from_rgb(226, 232, 240);
const DIM: Color32 = Color32::from_rgb(148, 163, 184);
const ACCENT: Color32 = Color32::from_rgb(129, 140, 248);
const GREEN: Color32 = Color32::from_rgb(52, 211, 153);
const AMBER: Color32 = Color32::from_rgb(245, 158, 11);
const BLUE: Color32 = Color32::from_rgb(96, 165, 250);

/// Open (or keep open) the dashboard window for one frame. Called from `App::ui` only
/// while `config.window.dashboard_open` is true; stopping the call closes the viewport.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.config.window.dashboard_open {
        return;
    }
    app.refresh_dashboard_tokens();
    let mut open = true;
    let viewport_id = egui::ViewportId::from_hash_of("mojiflow-dashboard");
    let builder = egui::ViewportBuilder::default()
        .with_title(crate::platform::DASHBOARD_TITLE)
        .with_inner_size(Vec2::new(880.0, 640.0))
        .with_min_inner_size(Vec2::new(560.0, 420.0))
        .with_transparent(false)
        .with_resizable(true);

    ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
        // Opaque root fill first: gaps between cards must never show the desktop
        // through (user review 2026-10-10).
        ui.painter().rect_filled(ui.max_rect(), 0.0, BG);
        // Per-viewport dark theme: the overlay's visuals are untouched. egui 0.36 styles
        // are per-`Ui`, so set the style on this viewport's root ui and children inherit.
        let style = ui.style_mut();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = BG;
        style.visuals.extreme_bg_color = BG;
        style.visuals.widgets.noninteractive.bg_fill = CARD_BG;
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(26, 30, 39);
        style.visuals.selection.bg_fill = Color32::from_rgb(88, 101, 242);
        style.visuals.hyperlink_color = ACCENT;

        // OS title-bar close button: stop showing the viewport next frame.
        if ui.input(|input| input.viewport().close_requested()) {
            open = false;
        }

        header(app, ui, &mut open);
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                clock_card(app, ui);
                subtitle_card(app, ui);
                export_card(app, ui);
                settings_card(app, ui);
                hotkeys_card(app, ui);
            });
    });

    app.config.window.dashboard_open = open;
}

/// Title row: app name, session chip, sync chip, last hotkey, close button.
fn header(app: &mut App, ui: &mut egui::Ui, open: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("MojiFlow").size(20.0).strong().color(TEXT));
        ui.label(RichText::new("control room").size(12.0).color(DIM));
        ui.add_space(14.0);

        chip(
            ui,
            detect_session().label(),
            Color32::from_rgb(28, 34, 50),
            ACCENT,
            None,
        );
        sync_chip(app, ui);

        // Last hotkey action, with a short fade so presses are visible.
        if let Some((at, label)) = app.last_action {
            let age = at.elapsed().as_secs_f32();
            if age < 2.5 {
                let alpha = ((1.0 - age / 2.5) * 180.0) as u8;
                chip(
                    ui,
                    &format!("last: {label}"),
                    Color32::from_rgb(40, 26, 30),
                    Color32::from_rgba_unmultiplied(248, 150, 120, alpha),
                    None,
                );
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("✕").color(DIM))
                        .min_size(Vec2::new(26.0, 24.0)),
                )
                .on_hover_text("Close the dashboard (Ctrl+Alt+D reopens)")
                .clicked()
            {
                *open = false;
            }
        });
    });
}

/// Green (live) / blue (local) / amber (stale) state chip, with the reason as a tooltip.
fn sync_chip(app: &mut App, ui: &mut egui::Ui) {
    let (label, bg, fg, tip) = match app.clock.sync_state() {
        SyncState::Live => (
            format!("live · {}", app.clock.source_name()),
            Color32::from_rgb(16, 42, 34),
            GREEN,
            None,
        ),
        SyncState::Local => (
            format!("local · {}", app.clock.source_name()),
            Color32::from_rgb(22, 34, 54),
            BLUE,
            None,
        ),
        SyncState::Stale(reason) => (
            "stale".to_owned(),
            Color32::from_rgb(44, 32, 16),
            AMBER,
            Some(reason),
        ),
    };
    chip(ui, &label, bg, fg, tip.as_deref());
}

fn clock_card(app: &mut App, ui: &mut egui::Ui) {
    render_card(ui, "Media clock", |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("source").color(DIM));
            chip(
                ui,
                source_label(app.config.clock.source),
                Color32::from_rgb(28, 34, 50),
                ACCENT,
                None,
            );
        });

        let now = app.clock.now();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format_timestamp(now.as_secs_f64()))
                    .size(28.0)
                    .strong()
                    .color(TEXT),
            );
            ui.label(
                RichText::new(if app.clock.is_playing() { "▶" } else { "⏸" })
                    .size(18.0)
                    .color(if app.clock.is_playing() { GREEN } else { DIM }),
            );
        });

        // Offset, always adjustable.
        ui.horizontal(|ui| {
            ui.label(RichText::new("offset").color(DIM));
            ui.label(
                RichText::new(format!("{:+.0} ms", app.user_offset_ms))
                    .color(if app.user_offset_ms == 0 { DIM } else { AMBER }),
            );
            let step = app.config.subtitle.offset_step_ms;
            if ui.button("−").clicked() {
                app.adjust_offset(-step);
            }
            if ui.button("+").clicked() {
                app.adjust_offset(step);
            }
        });

        ui.add_space(6.0);
        if app.config.clock.source == ClockSource::Manual {
            ui.horizontal(|ui| {
                if ui.button("⏮ −5 s").clicked() {
                    app.manual.seek_by(Duration::from_secs(5), false);
                }
                if ui.button("▶ / ⏸").clicked() {
                    app.manual.toggle_play_pause();
                }
                if ui.button("+5 s ⏭").clicked() {
                    app.manual.seek_by(Duration::from_secs(5), true);
                }
            });
            // "Start at": tell the clock how far into the video you are; it seeks there
            // paused and waits for Space, so subtitles start cleanly mid-video
            // (user review 2026-10-10).
            ui.horizontal(|ui| {
                ui.label(RichText::new("start at").color(DIM));
                ui.add(
                    egui::TextEdit::singleline(&mut app.start_at_seconds)
                        .hint_text("83.5 s")
                        .desired_width(90.0),
                );
                if ui.button("Seek").clicked() {
                    match app.start_at_seconds.trim().parse::<f64>() {
                        Ok(seconds) if seconds >= 0.0 && seconds.is_finite() => {
                            app.manual.seek_to(Duration::from_secs_f64(seconds));
                            app.manual.set_playing(false);
                        }
                        _ => {
                            app.toasts.push("enter seconds, e.g. 83.5".to_owned(), true);
                        }
                    }
                }
            });
        } else {
            ui.label(
                RichText::new(format!(
                    "'{}' is a live source (read-only transport) — switch sources in Settings below",
                    source_label(app.config.clock.source)
                ))
                .color(DIM),
            );
        }

        // Sync-help specific to the common setups (see AGENTS.md).
        match app.config.clock.source {
            ClockSource::Mpris => {
                ui.label(
                    RichText::new("automatic: reads a playing media player over MPRIS (Firefox tabs included); Chrome has no MPRIS, so it reads as stale — switch the source to Manual there")
                        .color(DIM)
                        .size(11.0),
                );
            }
            ClockSource::Manual => {
                ui.label(
                    RichText::new("press ▶ when the video starts, then nudge the offset +/- to align a line — the overlay follows the manual clock")
                        .color(DIM)
                        .size(11.0),
                );
            }
            _ => {}
        }
    });
}

fn subtitle_card(app: &mut App, ui: &mut egui::Ui) {
    render_card(ui, "Subtitle", |ui| {
        let Some(track) = app.display_track() else {
            ui.label(
                RichText::new("no subtitle track — Ctrl+Alt+O or drag a .srt onto the overlay")
                    .color(DIM),
            );
            return;
        };
        let name = track
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "subtitle".to_owned());
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).color(TEXT).strong());
            ui.label(
                RichText::new(format!(
                    "{} · {} · {} cues",
                    track.format().label(),
                    track.encoding(),
                    track.len()
                ))
                .color(DIM),
            );
        });

        match app.active_cue.and_then(|i| track.cue(i)) {
            Some(cue) => {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "[{} → {}]",
                        format_timestamp(cue.start.as_secs_f64()),
                        format_timestamp(cue.end.as_secs_f64())
                    ))
                    .color(ACCENT)
                    .monospace()
                    .size(12.0),
                );
                ui.label(RichText::new(&cue.text).size(15.0).color(TEXT));
                ui.add_space(6.0);

                // Tokens: green = resolved, amber = content word without an entry,
                // gray = particle/punctuation (drawn as plain labels).
                let rows = app
                    .dashboard_tokens
                    .as_ref()
                    .map(|(_, rows)| rows.clone())
                    .unwrap_or_default();
                ui.horizontal_wrapped(|ui| {
                    for row in &rows {
                        token_chip(ui, row);
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("green = dictionary hit · amber = content word, no entry")
                        .color(DIM)
                        .size(10.0),
                );
            }
            None => {
                // No nag line: an empty moment between cues is normal, not an error
                // (user review 2026-10-10).
            }
        }
    });
}

fn token_chip(ui: &mut egui::Ui, row: &TokenRow) {
    let (bg, fg, tip) = if row.resolved {
        (
            Color32::from_rgb(16, 42, 34),
            GREEN,
            format!("{} · dictionary entry", row.surface),
        )
    } else if row.content {
        (
            Color32::from_rgb(44, 32, 16),
            AMBER,
            format!("{} · no dictionary entry", row.surface),
        )
    } else {
        // Particle/punctuation: plain text, no chip.
        ui.label(RichText::new(&row.surface).color(Color32::from_rgb(100, 110, 124)));
        return;
    };
    let tip = if row.reading.is_empty() {
        tip
    } else {
        format!("{tip} · {reading}", reading = row.reading)
    };
    chip(ui, &row.surface, bg, fg, Some(&tip));
}

fn export_card(app: &mut App, ui: &mut egui::Ui) {
    render_card(ui, "Mining export", |ui| {
        let anki = &app.config.anki;
        ui.horizontal(|ui| {
            ui.label(RichText::new("anki").color(DIM));
            ui.label(RichText::new(&anki.url).color(TEXT).monospace());
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("deck → model").color(DIM));
            ui.label(
                RichText::new(format!("{} → {}", anki.deck, anki.model))
                    .color(TEXT)
                    .monospace(),
            );
        });

        ui.add_space(6.0);
        if ui
            .add(
                egui::Button::new(RichText::new("Export current card").strong().color(BG))
                    .fill(ACCENT)
                    .corner_radius(6),
            )
            .clicked()
        {
            app.export_current_card();
        }
        ui.add_space(6.0);

        if app.status_log.is_empty() {
            ui.label(RichText::new("no export activity yet").color(DIM));
        } else {
            ui.label(RichText::new("recent activity").color(DIM).size(11.0));
            for line in app.status_log.iter().rev() {
                ui.label(RichText::new(line).color(TEXT).monospace().size(11.0));
            }
        }
    });
}

fn settings_card(app: &mut App, ui: &mut egui::Ui) {
    use crate::config::ClockSource;

    render_card(ui, "Settings", |ui| {
        // Clock source with live switching (no restart needed).
        ui.horizontal(|ui| {
            ui.label(RichText::new("clock source").color(DIM));
            let mut selected = app.config.clock.source;
            egui::ComboBox::from_id_salt("clock-source")
                .selected_text(source_label(selected))
                .show_ui(ui, |ui| {
                    for source in ClockSource::ALL {
                        ui.selectable_value(&mut selected, source, source.label());
                    }
                });
            if selected != app.config.clock.source {
                app.switch_clock_source(selected);
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("offset step").color(DIM));
            ui.add(
                egui::DragValue::new(&mut app.config.subtitle.offset_step_ms)
                    .range(0..=5000)
                    .suffix(" ms"),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("anki url").color(DIM));
            ui.add(
                egui::TextEdit::singleline(&mut app.config.anki.url)
                    .hint_text("http://127.0.0.1:8765")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("explain endpoint").color(DIM));
            ui.add(
                egui::TextEdit::singleline(&mut app.config.explain.endpoint)
                    .hint_text("empty = disabled")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut app.config.window.always_on_top, "always on top");
            ui.checkbox(
                &mut app.config.window.follow_fullscreen,
                "follow fullscreen",
            );
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Save config").strong().color(BG))
                        .fill(ACCENT)
                        .corner_radius(6),
                )
                .clicked()
            {
                app.save_config();
            }
            match &app.config_path {
                Some(path) => ui.label(
                    RichText::new(path.display().to_string())
                        .color(DIM)
                        .size(11.0),
                ),
                None => ui.label(
                    RichText::new("no config file this run")
                        .color(DIM)
                        .size(11.0),
                ),
            }
        });
    });
}

fn hotkeys_card(app: &mut App, ui: &mut egui::Ui) {
    render_card(ui, "Hotkeys", |ui| {
        egui::Grid::new("hotkeys-global")
            .striped(true)
            .spacing(Vec2::new(24.0, 4.0))
            .show(ui, |ui| {
                for (spec, action) in configured_actions(&app.config.hotkeys) {
                    ui.label(RichText::new(action.label()).color(TEXT));
                    ui.label(RichText::new(spec).color(ACCENT).monospace());
                    ui.end_row();
                }
            });
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "bare keys (S, [, ]) work only while the overlay is focused / its popover is hovered — global bindings above fire anywhere on X11",
            )
            .color(DIM)
            .size(11.0),
        );
    });
}

fn source_label(source: ClockSource) -> &'static str {
    source.label()
}

/// Small rounded colored label, optional hover tooltip.
fn chip(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32, tip: Option<&str>) {
    let response = egui::Frame::NONE
        .fill(bg)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(fg).size(12.0));
        })
        .response;
    if let Some(tip) = tip {
        response.on_hover_text(tip);
    }
}

/// One titled card: rounded panel with an accent heading and a body.
fn render_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(CARD_BG)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(14))
        .stroke(Stroke::new(1.0, BORDER))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(13.0).strong().color(ACCENT));
            ui.add_space(6.0);
            body(ui);
        });
    ui.add_space(10.0);
}
