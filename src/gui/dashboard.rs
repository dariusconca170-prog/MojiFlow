//! Control-room dashboard: an opaque window beside the click-through overlay.
//!
//! Three-pane layout: a left nav rail (Control Room / Subtitle Track / Mining
//! History / Anki / Settings / Hotkeys), a center column with the session cards,
//! and a right inspector (word detail, session activity, quick keys). It is a
//! separate egui viewport opened via [`egui::Context::show_viewport_immediate`],
//! so it is a real OS window — never click-through, never transparent.
//!
//! The selected nav tab and the activity timestamps live in the viewport's egui
//! memory (no `App` fields needed); everything else reads live `App` state.

use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::App;
use crate::clock::SyncState;
use crate::config::ClockSource;
use crate::export::format_timestamp;
use crate::gui::popover::PopoverData;
use crate::hotkey::{configured_actions, local_actions};

// --- palette (dark indigo, matches the design mock) ---
const BG: Color32 = Color32::from_rgb(10, 14, 23);
const CARD_BG: Color32 = Color32::from_rgb(19, 25, 39);
const CHIP_BG: Color32 = Color32::from_rgb(30, 37, 54);
const BORDER: Color32 = Color32::from_rgb(35, 43, 61);
const TEXT: Color32 = Color32::from_rgb(226, 232, 240);
const DIM: Color32 = Color32::from_rgb(148, 163, 184);
const FAINT: Color32 = Color32::from_rgb(100, 110, 130);
const ACCENT: Color32 = Color32::from_rgb(99, 102, 241);
const ACCENT_LIGHT: Color32 = Color32::from_rgb(129, 140, 248);
const NAV_SELECTED: Color32 = Color32::from_rgb(46, 47, 105);
const GREEN: Color32 = Color32::from_rgb(52, 211, 153);
const GREEN_BG: Color32 = Color32::from_rgb(16, 42, 34);
const AMBER: Color32 = Color32::from_rgb(245, 158, 11);
const AMBER_BG: Color32 = Color32::from_rgb(44, 32, 16);
const BLUE: Color32 = Color32::from_rgb(96, 165, 250);
const BLUE_BG: Color32 = Color32::from_rgb(22, 34, 54);

const NAV_W: f32 = 208.0;
const INSP_W: f32 = 322.0;
const NAV_KEY: &str = "mojiflow-dash-nav";
const ACTIVITY_KEY: &str = "mojiflow-dash-activity";

/// One token of the active cue, as shown on the dashboard: is it content, and did the
/// dictionary resolve it?
#[derive(Debug, Clone)]
pub struct TokenRow {
    pub surface: String,
    pub reading: String,
    pub content: bool,
    pub resolved: bool,
}

/// Center-column page selected from the nav rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardNav {
    #[default]
    ControlRoom,
    Subtitles,
    Mining,
    Anki,
    Settings,
    Hotkeys,
}

impl DashboardNav {
    fn label(self) -> &'static str {
        match self {
            DashboardNav::ControlRoom => "Control Room",
            DashboardNav::Subtitles => "Subtitle Track",
            DashboardNav::Mining => "Mining History",
            DashboardNav::Anki => "Anki",
            DashboardNav::Settings => "Settings",
            DashboardNav::Hotkeys => "Hotkeys",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            DashboardNav::ControlRoom => "■",
            DashboardNav::Subtitles => "☰",
            DashboardNav::Mining => "◆",
            DashboardNav::Anki => "★",
            DashboardNav::Settings => "⚙",
            DashboardNav::Hotkeys => "⌨",
        }
    }
}

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
        .with_inner_size(Vec2::new(1420.0, 880.0))
        .with_min_inner_size(Vec2::new(1120.0, 680.0))
        .with_transparent(false)
        .with_resizable(true);

    ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
        paint_root(app, ui, &mut open);
    });

    app.config.window.dashboard_open = open;
}

fn paint_root(app: &mut App, ui: &mut egui::Ui, open: &mut bool) {
    ui.painter().rect_filled(ui.max_rect(), 0.0, BG);
    let style = ui.style_mut();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = BG;
    style.visuals.extreme_bg_color = BG;
    style.visuals.widgets.noninteractive.bg_fill = CARD_BG;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.weak_bg_fill = CHIP_BG;
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.hyperlink_color = ACCENT_LIGHT;

    if ui.input(|input| input.viewport().close_requested()) {
        *open = false;
    }

    let mut nav: DashboardNav = ui
        .memory(|memory| memory.data.get_temp(NAV_KEY.into()))
        .unwrap_or_default();

    top_bar(app, ui, open);
    ui.separator();

    let body_h = ui.available_height();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(NAV_W, body_h),
            Layout::top_down(Align::LEFT),
            |ui| nav_rail(app, ui, &mut nav),
        );
        ui.separator();
        let center_w = (ui.available_width() - INSP_W - 14.0).max(300.0);
        ui.allocate_ui_with_layout(
            Vec2::new(center_w, body_h),
            Layout::top_down(Align::LEFT),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("dash-center")
                    .auto_shrink([false, false])
                    .max_height(body_h)
                    .show(ui, |ui| {
                        ui.set_width(center_w - 14.0);
                        center_page(app, ui, nav);
                    });
            },
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            Vec2::new(INSP_W, body_h),
            Layout::top_down(Align::LEFT),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("dash-inspector")
                    .auto_shrink([false, false])
                    .max_height(body_h)
                    .show(ui, |ui| {
                        ui.set_width(INSP_W - 14.0);
                        inspector(app, ui);
                    });
            },
        );
    });

    ui.memory_mut(|memory| memory.data.insert_temp(NAV_KEY.into(), nav));
}

// --- top bar -----------------------------------------------------------------

fn top_bar(app: &mut App, ui: &mut egui::Ui, open: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("M").size(22.0).strong().color(ACCENT_LIGHT));
        ui.label(RichText::new("MojiFlow").size(17.0).strong().color(TEXT));
        ui.label(RichText::new("Japanese immersion").size(12.0).color(DIM));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("✕").color(DIM))
                        .min_size(Vec2::new(28.0, 26.0)),
                )
                .on_hover_text("Close the dashboard (Ctrl+Alt+D reopens)")
                .clicked()
            {
                *open = false;
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("▢").color(DIM))
                        .min_size(Vec2::new(28.0, 26.0)),
                )
                .on_hover_text("Maximize / restore")
                .clicked()
            {
                let maximized: bool = ui
                    .memory(|memory| memory.data.get_temp("mojiflow-dash-max".into()))
                    .unwrap_or(false);
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                ui.memory_mut(|memory| {
                    memory
                        .data
                        .insert_temp("mojiflow-dash-max".into(), !maximized);
                });
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("–").color(DIM))
                        .min_size(Vec2::new(28.0, 26.0)),
                )
                .on_hover_text("Minimize")
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            ui.add_space(10.0);
            overlay_pill(app, ui);
            source_pill(app, ui);
        });
    });
}

/// Green `MPRIS CONNECTED` / blue manual / amber stale pill.
fn source_pill(app: &App, ui: &mut egui::Ui) {
    let (label, bg, fg) = match app.clock.sync_state() {
        SyncState::Live => (
            format!("{} CONNECTED", app.clock.source_name().to_uppercase()),
            GREEN_BG,
            GREEN,
        ),
        SyncState::Local => (
            format!("{} CLOCK", app.clock.source_name().to_uppercase()),
            BLUE_BG,
            BLUE,
        ),
        SyncState::Stale(_) => ("SOURCE STALE".to_owned(), AMBER_BG, AMBER),
    };
    pill(ui, &label, bg, fg);
}

fn overlay_pill(app: &App, ui: &mut egui::Ui) {
    if app.visible {
        pill(ui, "OVERLAY ACTIVE", GREEN_BG, GREEN);
    } else {
        pill(ui, "OVERLAY HIDDEN", AMBER_BG, AMBER);
    }
}

// --- nav rail ------------------------------------------------------------------

fn nav_rail(app: &App, ui: &mut egui::Ui, nav: &mut DashboardNav) {
    ui.add_space(6.0);
    for tab in [
        DashboardNav::ControlRoom,
        DashboardNav::Subtitles,
        DashboardNav::Mining,
        DashboardNav::Anki,
    ] {
        nav_button(ui, tab, *nav == tab, nav);
    }
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    for tab in [DashboardNav::Settings, DashboardNav::Hotkeys] {
        nav_button(ui, tab, *nav == tab, nav);
    }

    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        ui.add_space(8.0);
        let explain_ready = !app.config.explain.endpoint.trim().is_empty();
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").color(if explain_ready { GREEN } else { FAINT }));
            ui.label(
                RichText::new(if explain_ready {
                    "llama.cpp ready"
                } else {
                    "llama.cpp off"
                })
                .size(11.0)
                .color(if explain_ready { GREEN } else { FAINT })
                .monospace(),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").size(11.0).color(FAINT));
            ui.label(
                RichText::new("LOCAL · PRIVATE")
                    .size(11.0)
                    .color(FAINT)
                    .monospace(),
            );
        });
    });
}

fn nav_button(ui: &mut egui::Ui, tab: DashboardNav, selected: bool, nav: &mut DashboardNav) {
    let fill = if selected {
        NAV_SELECTED
    } else {
        Color32::TRANSPARENT
    };
    let text_color = if selected { TEXT } else { DIM };
    let button = egui::Button::new(
        RichText::new(format!("{}  {}", tab.icon(), tab.label()))
            .size(14.0)
            .color(text_color),
    )
    .fill(fill)
    .corner_radius(8)
    .min_size(Vec2::new(NAV_W - 16.0, 36.0));
    if ui.add(button).clicked() {
        *nav = tab;
    }
}

// --- center pages ---------------------------------------------------------------

fn center_page(app: &mut App, ui: &mut egui::Ui, nav: DashboardNav) {
    match nav {
        DashboardNav::ControlRoom => control_room(app, ui),
        DashboardNav::Subtitles => subtitles_page(app, ui),
        DashboardNav::Mining => mining_page(app, ui),
        DashboardNav::Anki => anki_page(app, ui),
        DashboardNav::Settings => settings_page(app, ui),
        DashboardNav::Hotkeys => hotkeys_page(app, ui),
    }
}

fn control_room(app: &mut App, ui: &mut egui::Ui) {
    page_title(ui, "■", "Control Room", "Your watch-and-mine session");
    media_card(app, ui);
    subtitle_card(app, ui);
    ui.columns(2, |cols| {
        audio_card(app, &mut cols[0]);
        anki_mini_card(app, &mut cols[1]);
    });
    queue_card(app, ui);
}

fn page_title(ui: &mut egui::Ui, icon: &str, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(26.0).color(TEXT));
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(24.0).strong().color(TEXT));
            ui.label(RichText::new(subtitle).size(12.0).color(DIM));
        });
    });
    ui.add_space(4.0);
}

// --- media clock card ------------------------------------------------------------

fn source_detail(app: &App) -> String {
    match app.config.clock.source {
        ClockSource::Mpris => {
            if app.config.clock.mpris_player.trim().is_empty() {
                "MPRIS · auto-detect".to_owned()
            } else {
                app.config.clock.mpris_player.clone()
            }
        }
        ClockSource::MpvIpc => app.config.clock.mpv_socket.clone(),
        ClockSource::Manual => "Manual clock".to_owned(),
        ClockSource::WhisperLive => "Live transcription".to_owned(),
    }
}

fn media_card(app: &mut App, ui: &mut egui::Ui) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(source_detail(app))
                    .size(13.0)
                    .strong()
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                sync_pill(app, ui);
            });
        });
        ui.add_space(4.0);

        ui.horizontal_top(|ui| {
            let playing = app.clock.is_playing();
            let glyph = if playing { "||" } else { "▶" };
            let play = egui::Button::new(RichText::new(glyph).size(20.0).color(Color32::WHITE))
                .fill(ACCENT)
                .corner_radius(28)
                .min_size(Vec2::new(56.0, 56.0));
            if ui
                .add(play)
                .on_hover_text("Start / pause the manual clock")
                .clicked()
            {
                if app.transport_available() {
                    app.manual.toggle_play_pause();
                } else {
                    app.report_live_transport();
                }
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                let now = app.clock.now();
                ui.label(
                    RichText::new(format_timestamp(now.as_secs_f64()))
                        .size(30.0)
                        .strong()
                        .monospace()
                        .color(TEXT),
                );
                let frac = cue_progress(app);
                ui.add_sized(
                    Vec2::new(ui.available_width(), 10.0),
                    egui::ProgressBar::new(frac).fill(ACCENT_LIGHT),
                );
            });
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Offset").size(12.0).color(DIM));
                    ui.label(
                        RichText::new(format!("{:+} ms", app.user_offset_ms))
                            .size(12.0)
                            .monospace()
                            .color(if app.user_offset_ms == 0 { DIM } else { AMBER }),
                    );
                });
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("−").color(TEXT))
                                .min_size(Vec2::new(32.0, 28.0)),
                        )
                        .clicked()
                    {
                        let step = app.config.subtitle.offset_step_ms;
                        app.adjust_offset(-step);
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("+").color(TEXT))
                                .min_size(Vec2::new(32.0, 28.0)),
                        )
                        .clicked()
                    {
                        let step = app.config.subtitle.offset_step_ms;
                        app.adjust_offset(step);
                    }
                });
            });
        });

        // Same predicate the hotkeys use, so the dashboard and `Ctrl+Alt+Space` never
        // disagree about whether transport is available.
        if app.transport_available() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("<< −5 s").clicked() {
                    app.manual.seek_by(std::time::Duration::from_secs(5), false);
                }
                if ui.button("▶ / ||").clicked() {
                    app.manual.toggle_play_pause();
                }
                if ui.button("+5 s >>").clicked() {
                    app.manual.seek_by(std::time::Duration::from_secs(5), true);
                }
                ui.label(RichText::new("start at").color(DIM).size(12.0));
                ui.add(
                    egui::TextEdit::singleline(&mut app.start_at_seconds)
                        .hint_text("83.5 s")
                        .desired_width(80.0),
                );
                if ui.button("Seek").clicked() {
                    match app.start_at_seconds.trim().parse::<f64>() {
                        Ok(seconds) if seconds >= 0.0 && seconds.is_finite() => {
                            app.manual
                                .seek_to(std::time::Duration::from_secs_f64(seconds));
                            app.manual.set_playing(false);
                        }
                        _ => {
                            app.toasts.push("enter seconds, e.g. 83.5".to_owned(), true);
                        }
                    }
                }
            });
        }
    });
}

fn sync_pill(app: &App, ui: &mut egui::Ui) {
    match app.clock.sync_state() {
        SyncState::Live => pill(ui, "● SYNCED", GREEN_BG, GREEN),
        SyncState::Local => pill(ui, "● LOCAL", BLUE_BG, BLUE),
        SyncState::Stale(reason) => {
            pill(ui, "● STALE", AMBER_BG, AMBER);
            ui.label(RichText::new(reason).size(11.0).color(FAINT));
        }
    }
}

/// Fraction of the active cue elapsed (0 when no cue is active).
fn cue_progress(app: &App) -> f32 {
    let (Some(track), Some(index)) = (app.display_track(), app.active_cue) else {
        return 0.0;
    };
    let Some(cue) = track.cue(index) else {
        return 0.0;
    };
    let start = cue.start.as_secs_f64();
    let end = cue.end.as_secs_f64();
    if end <= start {
        return 0.0;
    }
    let effective =
        crate::clock::effective_time(app.clock.as_ref(), app.user_offset_ms).as_secs_f64();
    ((effective - start) / (end - start)).clamp(0.0, 1.0) as f32
}

// --- subtitle card ---------------------------------------------------------------

fn subtitle_card(app: &mut App, ui: &mut egui::Ui) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("☰").size(15.0).color(ACCENT_LIGHT));
            ui.label(
                RichText::new("Current subtitle")
                    .size(14.0)
                    .strong()
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("···")
                    .on_hover_text("Open a subtitle file")
                    .clicked()
                {
                    // Same path as Ctrl+Alt+O — a button labelled "open a subtitle file"
                    // must not just tell the user to press a different key.
                    app.open_subtitle_dialog();
                }
                if ui
                    .small_button("Copy")
                    .on_hover_text("Copy the active line")
                    .clicked()
                {
                    let text = app.subtitle_text();
                    if text.is_empty() {
                        app.toasts.push("no active line to copy".to_owned(), true);
                    } else {
                        ui.ctx().copy_text(text);
                        app.toasts.push("line copied".to_owned(), false);
                    }
                }
            });
        });
        ui.add_space(4.0);

        let Some(track) = app.display_track() else {
            ui.label(
                RichText::new("No subtitle track — Ctrl+Alt+O or drag a .srt onto the overlay.")
                    .color(DIM)
                    .size(13.0),
            );
            return;
        };
        let rows = app
            .dashboard_tokens
            .as_ref()
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default();
        // The borrow below ends at its last use (NLL), freeing `app` for the chips.
        let (format_label, encoding) = (track.format().label(), track.encoding().to_owned());
        if rows.is_empty() {
            ui.label(RichText::new("…").color(FAINT).size(16.0));
        } else {
            token_chips(app, ui, &rows);
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "{} tokens · {format_label} · {encoding}",
                rows.len()
            ))
            .size(11.0)
            .color(FAINT),
        );
    });
}

/// Index of the chip the inspector follows: pinned, hovered, first resolved hit,
/// else first content word.
fn selected_row(app: &App, rows: &[TokenRow]) -> Option<usize> {
    app.hover_token
        .or(app.pinned_token)
        .filter(|index| *index < rows.len())
        .or_else(|| rows.iter().position(|row| row.content && row.resolved))
        .or_else(|| rows.iter().position(|row| row.content))
}

fn token_chips(app: &mut App, ui: &mut egui::Ui, rows: &[TokenRow]) {
    let selected = selected_row(app, rows);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        for (index, row) in rows.iter().enumerate() {
            let is_selected = selected == Some(index);
            let (bg, fg) = if is_selected {
                (ACCENT, Color32::WHITE)
            } else if row.content && row.resolved {
                (CHIP_BG, TEXT)
            } else if row.content {
                (CHIP_BG, AMBER)
            } else {
                (CHIP_BG, DIM)
            };
            let tip = if row.reading.is_empty() {
                row.surface.clone()
            } else {
                format!("{} · {}", row.surface, row.reading)
            };
            let response = ui
                .add(
                    egui::Button::new(RichText::new(&row.surface).size(19.0).color(fg))
                        .fill(bg)
                        .corner_radius(8)
                        .min_size(Vec2::new(44.0, 44.0)),
                )
                .on_hover_text(tip);
            if response.clicked() {
                app.pinned_token = if app.pinned_token == Some(index) {
                    None
                } else {
                    Some(index)
                };
            }
        }
    });
}

// --- audio + anki mini cards -------------------------------------------------------

fn audio_card(app: &App, ui: &mut egui::Ui) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("♪").size(15.0).color(ACCENT_LIGHT));
            ui.label(
                RichText::new("Audio capture")
                    .size(14.0)
                    .strong()
                    .color(TEXT),
            );
        });
        ui.add_space(4.0);
        waveform(ui, &waveform_bars(app));
        ui.add_space(4.0);
        let (device, rate, error) = app
            .audio
            .as_ref()
            .map(|audio| {
                let status = audio.status();
                (status.device, status.sample_rate, status.last_error)
            })
            .unwrap_or_default();
        let buffered = app
            .audio
            .as_ref()
            .and_then(|audio| audio.ring())
            .map(|ring| ring.buffered_seconds())
            .unwrap_or(0.0);
        match error {
            Some(err) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(AMBER));
                    ui.label(
                        RichText::new(format!("retrying… {err}"))
                            .size(12.0)
                            .color(AMBER),
                    );
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(GREEN));
                    ui.label(
                        RichText::new(format!("{buffered:.1} s buffer ready"))
                            .size(12.0)
                            .color(TEXT),
                    );
                });
            }
        }
        if !device.is_empty() {
            ui.label(
                RichText::new(format!("{device} @ {rate} Hz"))
                    .size(11.0)
                    .color(FAINT),
            );
        }
    });
}

/// Peak-normalized energy bars for the tail of the capture ring.
fn waveform_bars(app: &App) -> Vec<f32> {
    const BARS: usize = 84;
    let Some(ring) = app.audio.as_ref().and_then(|audio| audio.ring()) else {
        return vec![0.0; BARS];
    };
    let horizon = ring.written();
    if horizon == 0 {
        return vec![0.0; BARS];
    }
    let window = horizon.min(ring.sample_rate() as usize * 8);
    let start = horizon.saturating_sub(window);
    let Ok(samples) = ring.read(start, horizon) else {
        return vec![0.0; BARS];
    };
    if samples.is_empty() {
        return vec![0.0; BARS];
    }
    let per_bucket = (samples.len() / BARS).max(1);
    let mut bars = Vec::with_capacity(BARS);
    for chunk in samples.chunks(per_bucket) {
        let peak = chunk
            .iter()
            .fold(0.0_f32, |max, sample| max.max(sample.abs()));
        bars.push(peak);
    }
    bars.resize(BARS, 0.0);
    let max = bars
        .iter()
        .fold(0.0_f32, |max, value| max.max(*value))
        .max(1e-6);
    for value in &mut bars {
        *value = (*value / max).clamp(0.03, 1.0);
    }
    bars
}

fn waveform(ui: &mut egui::Ui, bars: &[f32]) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 64.0), egui::Sense::hover());
    let painter = ui.painter();
    let count = bars.len().max(1);
    let slot = rect.width() / count as f32;
    let bar_w = (slot - 2.0).max(1.5);
    for (index, value) in bars.iter().enumerate() {
        let height = (*value * (rect.height() - 8.0)).max(2.0);
        let center_y = rect.center().y;
        let min = egui::Pos2::new(
            rect.left() + index as f32 * slot + 1.0,
            center_y - height / 2.0,
        );
        painter.rect_filled(
            egui::Rect::from_min_size(min, Vec2::new(bar_w, height)),
            1.5,
            ACCENT_LIGHT,
        );
    }
}

fn anki_health(app: &App) -> (&'static str, Color32, Color32) {
    let failed = app
        .status_log
        .iter()
        .rev()
        .any(|line| line.contains("queued offline") || line.contains("failed"));
    let ok = app
        .status_log
        .iter()
        .any(|line| line.contains("exported") || line.contains("AnkiConnect available"));
    if failed && !ok {
        ("Unreachable", AMBER_BG, AMBER)
    } else {
        ("Connected", GREEN_BG, GREEN)
    }
}

fn anki_mini_card(app: &mut App, ui: &mut egui::Ui) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("★").size(15.0).color(ACCENT_LIGHT));
            ui.label(RichText::new("Anki export").size(14.0).strong().color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, bg, fg) = anki_health(app);
                pill(ui, &format!("● {label}"), bg, fg);
            });
        });
        ui.add_space(4.0);
        ui.label(RichText::new("Mapping").size(11.0).color(DIM));
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&app.config.anki.deck).size(12.0).color(TEXT));
            ui.label(RichText::new("→").size(12.0).color(FAINT));
            ui.label(RichText::new(&app.config.anki.model).size(12.0).color(TEXT));
        });
        ui.add_space(6.0);
        let export = egui::Button::new(
            RichText::new("↑   Export current card")
                .size(14.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(ACCENT)
        .corner_radius(8)
        .min_size(Vec2::new(ui.available_width(), 42.0));
        if ui.add(export).clicked() {
            app.export_current_card();
        }
        ui.add_space(6.0);
        // Sentence cards: the whole line as the expression, with its kana reading, its audio
        // and — when the local LLM endpoint is set — a translation in the glossary.
        let sentence = egui::Button::new(
            RichText::new("≡   Export whole line")
                .size(14.0)
                .strong()
                .color(TEXT),
        )
        .fill(CHIP_BG)
        .corner_radius(8)
        .min_size(Vec2::new(ui.available_width(), 34.0));
        if ui.add(sentence).clicked() {
            app.export_current_sentence();
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "AnkiConnect · {}",
                app.config
                    .anki
                    .url
                    .trim_start_matches("http://")
                    .trim_start_matches("https://")
            ))
            .size(11.0)
            .color(FAINT)
            .monospace(),
        );
    });
}

/// Notes that landed while Anki was unreachable, with the one action that helps: retry now.
/// Before this card the queue was invisible — a single toast line at export time was the
/// only sign it existed, and nothing said how many notes were waiting or for how long.
fn queue_card(app: &mut App, ui: &mut egui::Ui) {
    let status = app.queue_status();
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("⇅").size(15.0).color(ACCENT_LIGHT));
            ui.label(
                RichText::new("Offline queue")
                    .size(14.0)
                    .strong()
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if status.count() == 0 {
                    pill(ui, "● Empty", GREEN_BG, GREEN);
                } else {
                    pill(
                        ui,
                        &format!("● {} waiting", status.count()),
                        AMBER_BG,
                        AMBER,
                    );
                }
            });
        });
        ui.add_space(4.0);
        if status.count() == 0 {
            ui.label(
                RichText::new("Everything mined so far reached Anki.")
                    .size(12.0)
                    .color(DIM),
            );
        } else {
            for row in status.rows.iter().take(4) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&row.preview).size(12.0).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let age = row
                            .queued
                            .map(|age| format!("waiting {}", short_duration(age)))
                            .unwrap_or_else(|| "waiting".to_owned());
                        ui.label(RichText::new(age).size(11.0).color(FAINT));
                    });
                });
            }
            if status.rows.len() > 4 {
                ui.label(
                    RichText::new(format!("+{} more", status.rows.len() - 4))
                        .size(11.0)
                        .color(FAINT),
                );
            }
        }
        if status.unreadable > 0 {
            ui.label(
                RichText::new(format!(
                    "{} unreadable file(s) kept in {}",
                    status.unreadable,
                    status.dir.display()
                ))
                .size(11.0)
                .color(AMBER),
            );
        }
        ui.add_space(6.0);
        let retry = egui::Button::new(RichText::new("↻   Retry now").size(13.0).color(TEXT))
            .fill(CHIP_BG)
            .corner_radius(8)
            .min_size(Vec2::new(ui.available_width(), 30.0));
        if ui.add(retry).clicked() && !app.retry_offline_queue() {
            app.toasts.push(
                "export worker is busy — try again in a moment".to_owned(),
                true,
            );
        }
    });
}

/// `95s` / `4m` / `2h` — short enough for a card row.
fn short_duration(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

// --- inspector (right rail) ----------------------------------------------------------

fn inspector(app: &mut App, ui: &mut egui::Ui) {
    word_inspector(app, ui);
    activity_card(app, ui);
    quick_keys(app, ui);
}

fn word_inspector(app: &mut App, ui: &mut egui::Ui) {
    card(ui, |ui| {
        section_header(ui, "≡", "WORD INSPECTOR");
        let rows = app
            .dashboard_tokens
            .as_ref()
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default();
        let Some(index) = selected_row(app, &rows) else {
            ui.label(
                RichText::new("Hover a token in the overlay or the subtitle card.")
                    .size(12.0)
                    .color(DIM),
            );
            return;
        };
        let Some(row) = rows.get(index) else {
            return;
        };
        let data: PopoverData = app.lookup_popover(&row.surface);
        let sentence = app.subtitle_text();

        if data.note.is_none() {
            pill(ui, "Dictionary hit", GREEN_BG, GREEN);
        } else {
            pill(ui, "No entry", AMBER_BG, AMBER);
        }
        ui.label(RichText::new(&data.term).size(30.0).strong().color(TEXT));
        if !data.reading.is_empty() {
            ui.label(RichText::new(&data.reading).size(15.0).color(DIM));
        }
        if !data.pos.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for pos in &data.pos {
                    pill(ui, &pos.to_uppercase(), CHIP_BG, DIM);
                }
            });
        }
        if !data.glosses.is_empty() {
            ui.label(
                RichText::new(data.glosses.join("  ·  "))
                    .size(13.0)
                    .color(TEXT),
            );
        }
        if let Some(chain) = &data.chain {
            ui.label(RichText::new(chain).size(11.0).color(FAINT).monospace());
        }
        ui.add_space(4.0);

        if let Some(pitch) = &data.pitch {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Pitch accent").size(12.0).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(pattern_name(pitch, &data.reading))
                            .size(12.0)
                            .color(TEXT),
                    );
                });
            });
            pitch_graph(ui, pitch);
        }
        if let Some(rank) = data.frequency_rank {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Frequency").size(12.0).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(format_rank(rank)).size(12.0).color(TEXT));
                });
            });
        }

        ui.add_space(4.0);
        let key = format!("{}｜{}", sentence, data.term);
        let busy = app.explaining.as_deref() == Some(key.as_str());
        let explain = egui::Button::new(
            RichText::new(if busy {
                "Explaining…"
            } else {
                "★   Explain with local AI"
            })
            .size(13.0)
            .color(TEXT),
        )
        .fill(CHIP_BG)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(8)
        .min_size(Vec2::new(ui.available_width(), 36.0));
        if ui.add(explain).clicked() && !busy {
            app.request_explanation_for(sentence, data.term.clone(), data.reading.clone());
        }
        if let Some(answer) = app.explanations.get(&key).cloned() {
            ui.add_space(4.0);
            ui.label(RichText::new(answer).size(13.0).color(TEXT));
        } else if app.config.explain.endpoint.trim().is_empty() {
            ui.label(
                RichText::new("Set [explain] endpoint in Settings to enable.")
                    .size(11.0)
                    .color(FAINT),
            );
        }
    });
}

/// Rendered contour (`￣_ [1]`) → per-mora high/low flags.
fn pitch_contour(pitch: &str) -> Vec<bool> {
    pitch
        .split('[')
        .next()
        .unwrap_or("")
        .chars()
        .filter_map(|mark| match mark {
            '￣' => Some(true),
            '_' => Some(false),
            _ => None,
        })
        .collect()
}

fn pattern_name(pitch: &str, reading: &str) -> &'static str {
    let number: Option<u8> = pitch
        .rsplit('[')
        .next()
        .and_then(|tail| tail.trim_end_matches(']').trim().parse().ok());
    match number {
        Some(0) => "Heiban",
        Some(1) => "Atamadaka",
        Some(n) => {
            let moras = crate::gui::popover::moras(reading).len().max(1);
            if usize::from(n) >= moras {
                "Odaka"
            } else {
                "Nakadaka"
            }
        }
        None => "",
    }
}

fn pitch_graph(ui: &mut egui::Ui, pitch: &str) {
    let contour = pitch_contour(pitch);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 30.0), egui::Sense::hover());
    if contour.is_empty() {
        return;
    }
    let painter = ui.painter();
    let y_high = rect.top() + 7.0;
    let y_low = rect.bottom() - 7.0;
    let pad = 10.0;
    let step = if contour.len() > 1 {
        (rect.width() - pad * 2.0) / (contour.len() - 1) as f32
    } else {
        0.0
    };
    let points: Vec<egui::Pos2> = contour
        .iter()
        .enumerate()
        .map(|(index, high)| {
            egui::Pos2::new(
                rect.left() + pad + index as f32 * step,
                if *high { y_high } else { y_low },
            )
        })
        .collect();
    for pair in points.windows(2) {
        painter.line_segment([pair[0], pair[1]], Stroke::new(2.0, ACCENT_LIGHT));
    }
    for point in &points {
        painter.circle_filled(*point, 3.5, ACCENT_LIGHT);
    }
}

fn format_rank(rank: i64) -> String {
    let digits: Vec<char> = rank.to_string().chars().rev().collect();
    let mut grouped = String::new();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*digit);
    }
    let grouped: String = grouped.chars().rev().collect();
    if rank <= 10_000 {
        format!("Top {grouped}")
    } else {
        format!("#{grouped}")
    }
}

// --- session activity + quick keys -----------------------------------------------------

/// Timestamped mirror of `App.status_log` kept in viewport memory (newest first).
#[derive(Debug, Clone, Default)]
struct ActivityLog {
    entries: Vec<(String, String)>,
}

fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

fn activity_card(app: &App, ui: &mut egui::Ui) {
    let mut log: ActivityLog = ui
        .memory(|memory| memory.data.get_temp(ACTIVITY_KEY.into()))
        .unwrap_or_default();
    for line in app.status_log.iter() {
        if !log.entries.iter().any(|(_, text)| text == line) {
            log.entries.insert(0, (now_hms(), line.clone()));
        }
    }
    log.entries.truncate(8);
    ui.memory_mut(|memory| memory.data.insert_temp(ACTIVITY_KEY.into(), log.clone()));

    card(ui, |ui| {
        section_header(ui, "•", "SESSION ACTIVITY");
        if log.entries.is_empty() {
            ui.label(RichText::new("No activity yet.").size(12.0).color(FAINT));
            return;
        }
        for (time, text) in &log.entries {
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(10.0).color(GREEN));
                ui.label(RichText::new(text).size(12.0).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(time).size(11.0).monospace().color(FAINT));
                });
            });
        }
    });
}

fn quick_keys(app: &App, ui: &mut egui::Ui) {
    card(ui, |ui| {
        section_header(ui, "⌨", "QUICK KEYS");
        let hotkeys = &app.config.hotkeys;
        for (label, spec) in [
            ("Export card", hotkeys.export_card.as_str()),
            ("Export line", hotkeys.export_sentence.as_str()),
            ("Pause clock", hotkeys.clock_start_pause.as_str()),
            ("Open subtitle", hotkeys.open_subtitle.as_str()),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).size(12.0).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    kbd(ui, spec);
                });
            });
        }
    });
}

// --- other center pages ------------------------------------------------------------------

fn subtitles_page(app: &mut App, ui: &mut egui::Ui) {
    page_title(ui, "☰", "Subtitle Track", "Cues around the playhead");
    let Some(track) = app.display_track() else {
        card(ui, |ui| {
            ui.label(
                RichText::new("No subtitle track — Ctrl+Alt+O or drag a .srt onto the overlay.")
                    .color(DIM)
                    .size(13.0),
            );
        });
        return;
    };
    let name = track
        .path()
        .file_name()
        .map(|file| file.to_string_lossy().into_owned())
        .unwrap_or_else(|| "subtitle".to_owned());
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).strong().color(TEXT).size(13.0));
            ui.label(
                RichText::new(format!(
                    "{} · {} · {} cues",
                    track.format().label(),
                    track.encoding(),
                    track.len()
                ))
                .size(12.0)
                .color(DIM),
            );
        });
    });
    let active = app.active_cue.unwrap_or(0);
    let first = active.saturating_sub(40);
    let last = (active + 60).min(track.len());
    card(ui, |ui| {
        for index in first..last {
            let Some(cue) = track.cue(index) else {
                continue;
            };
            let label = format!(
                "{} → {}  {}",
                format_timestamp(cue.start.as_secs_f64()),
                format_timestamp(cue.end.as_secs_f64()),
                cue.text.replace('\n', " "),
            );
            let response = ui.selectable_label(index == active, RichText::new(label).size(12.0));
            if response.clicked() && app.transport_available() {
                app.manual.seek_to(cue.start);
            }
        }
    });
}

fn mining_page(app: &mut App, ui: &mut egui::Ui) {
    page_title(ui, "◆", "Mining History", "Recent exports this session");
    card(ui, |ui| {
        let export = egui::Button::new(
            RichText::new("↑   Export current card")
                .size(14.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(ACCENT)
        .corner_radius(8)
        .min_size(Vec2::new(ui.available_width(), 42.0));
        if ui.add(export).clicked() {
            app.export_current_card();
        }
        ui.add_space(6.0);
        if app.status_log.is_empty() {
            ui.label(
                RichText::new("No export activity yet.")
                    .color(DIM)
                    .size(12.0),
            );
        } else {
            for line in app.status_log.iter().rev() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").size(10.0).color(GREEN));
                    ui.label(RichText::new(line).size(12.0).color(TEXT).monospace());
                });
            }
        }
    });
}

fn anki_page(app: &mut App, ui: &mut egui::Ui) {
    page_title(ui, "★", "Anki", "Deck, model and field mapping");
    card(ui, |ui| {
        field_row(
            ui,
            "AnkiConnect URL",
            &mut app.config.anki.url,
            "http://127.0.0.1:8765",
        );
        field_row(ui, "Deck", &mut app.config.anki.deck, "Japanese::Mining");
        field_row(
            ui,
            "Model",
            &mut app.config.anki.model,
            "Japanese Mining (medialingual)",
        );
        field_row(
            ui,
            "Duplicate scope",
            &mut app.config.anki.duplicate_scope,
            "deck",
        );
        ui.horizontal(|ui| {
            ui.label(RichText::new("Queue retry").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.anki.retry_interval_s)
                    .range(ranges::RETRY_INTERVAL_S)
                    .suffix(" s"),
            )
            .on_hover_text("How often to retry queued notes while Anki is unreachable (0 = off)");
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Field mapping")
                .size(12.0)
                .strong()
                .color(TEXT),
        );
        let keys: Vec<String> = app.config.anki.field_mapping.keys().cloned().collect();
        for key in keys {
            if let Some(value) = app.config.anki.field_mapping.get_mut(&key) {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&key)
                            .size(12.0)
                            .monospace()
                            .color(ACCENT_LIGHT),
                    );
                    ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
                });
            }
        }
        ui.add_space(6.0);
        if ui
            .add(
                egui::Button::new(RichText::new("Save config").strong().color(Color32::WHITE))
                    .fill(ACCENT)
                    .corner_radius(8),
            )
            .clicked()
        {
            app.save_config();
        }
    });
}

/// Ranges the settings card exposes.
///
/// Constants so [`tests::settings_ranges_stay_inside_the_validator`] can prove a widget can
/// never produce a config that `config::Validator` rejects — the exact failure mode behind
/// the offset-step bug (a widget allowed 0, Save then refused, and the app toasted on every
/// later launch).
mod ranges {
    pub const FONT_SIZE: std::ops::RangeInclusive<f32> = 8.0..=160.0;
    pub const OFFSET_STEP_MS: std::ops::RangeInclusive<i64> = 1..=5000;
    pub const BUFFER_SECONDS: std::ops::RangeInclusive<f32> = 1.0..=120.0;
    pub const PADDING_MS: std::ops::RangeInclusive<u32> = 0..=2_000;
    pub const BITRATE_KBPS: std::ops::RangeInclusive<u32> = 32..=320;
    pub const LIVE_DELAY_MS: std::ops::RangeInclusive<i64> = -5_000..=5_000;
    pub const RETRY_INTERVAL_S: std::ops::RangeInclusive<u64> = 0..=3_600;
}

fn settings_page(app: &mut App, ui: &mut egui::Ui) {
    page_title(ui, "⚙", "Settings", "Live-switchable, saved on demand");
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Clock source").color(DIM).size(12.0));
            let mut selected = app.config.clock.source;
            egui::ComboBox::from_id_salt("clock-source")
                .selected_text(selected.label())
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
            ui.label(RichText::new("Offset step").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.subtitle.offset_step_ms)
                    .range(ranges::OFFSET_STEP_MS)
                    .suffix(" ms"),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("MPRIS player").color(DIM).size(12.0));
            ui.add(
                egui::TextEdit::singleline(&mut app.config.clock.mpris_player)
                    .hint_text("empty = auto-detect")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Live cue delay").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.clock.live_cue_delay_ms)
                    .range(ranges::LIVE_DELAY_MS)
                    .suffix(" ms"),
            )
            .on_hover_text(
                "Live transcription alignment: shifts whisper-live cues onto the video \
                 (+ later, − earlier)",
            );
        });
    });
    card(ui, |ui| {
        ui.label(RichText::new("Subtitles").strong().color(TEXT).size(13.0));
        ui.horizontal(|ui| {
            ui.label(RichText::new("Font size").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.subtitle.font_size)
                    .range(ranges::FONT_SIZE)
                    .speed(0.5),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Position").color(DIM).size(12.0));
            let label = app.config.subtitle.position.replace('_', " ");
            egui::ComboBox::from_id_salt("subtitle-position")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for value in ["bottom_center", "top_center", "custom"] {
                        ui.selectable_value(
                            &mut app.config.subtitle.position,
                            value.to_owned(),
                            value.replace('_', " "),
                        );
                    }
                });
            if app.config.subtitle.position == "custom" {
                ui.add(
                    egui::Slider::new(&mut app.config.subtitle.custom_y, 0.0..=1.0)
                        .show_value(false),
                )
                .on_hover_text("Vertical position (0 = top, 1 = bottom)");
            }
        });
        ui.checkbox(
            &mut app.config.subtitle.show_furigana,
            "Furigana above the kanji",
        );
    });
    card(ui, |ui| {
        ui.label(RichText::new("Audio").strong().color(TEXT).size(13.0));
        ui.horizontal(|ui| {
            ui.label(RichText::new("Device").color(DIM).size(12.0));
            ui.add(
                egui::TextEdit::singleline(&mut app.config.audio.device)
                    .hint_text("empty = first monitor source")
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Buffer").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.audio.buffer_seconds)
                    .range(ranges::BUFFER_SECONDS)
                    .suffix(" s"),
            );
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Clip padding").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.audio.padding_ms)
                    .range(ranges::PADDING_MS)
                    .suffix(" ms"),
            )
            .on_hover_text("Audio added around each cue when exporting a card");
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Export bitrate").color(DIM).size(12.0));
            ui.add(
                egui::DragValue::new(&mut app.config.audio.mp3_bitrate_kbps)
                    .range(ranges::BITRATE_KBPS)
                    .suffix(" kbps"),
            );
            ui.add_space(8.0);
            ui.checkbox(&mut app.config.audio.normalize, "Normalize clip");
        });
    });
    card(ui, |ui| {
        ui.label(RichText::new("Explanation").strong().color(TEXT).size(13.0));
        field_row(
            ui,
            "Endpoint",
            &mut app.config.explain.endpoint,
            "empty = disabled",
        );
        field_row(ui, "Model", &mut app.config.explain.model, "server default");
    });
    card(ui, |ui| {
        ui.label(RichText::new("Window").strong().color(TEXT).size(13.0));
        ui.horizontal(|ui| {
            ui.checkbox(&mut app.config.window.always_on_top, "Always on top");
            ui.checkbox(
                &mut app.config.window.follow_fullscreen,
                "Follow fullscreen",
            );
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Save config").strong().color(Color32::WHITE))
                        .fill(ACCENT)
                        .corner_radius(8),
                )
                .clicked()
            {
                app.save_config();
            }
            match &app.config_path {
                Some(path) => ui.label(
                    RichText::new(path.display().to_string())
                        .color(FAINT)
                        .size(11.0),
                ),
                None => ui.label(
                    RichText::new("no config file this run")
                        .color(FAINT)
                        .size(11.0),
                ),
            };
        });
    });
}

fn hotkeys_page(app: &App, ui: &mut egui::Ui) {
    page_title(ui, "⌨", "Hotkeys", "Global bindings fire anywhere on X11");
    card(ui, |ui| {
        egui::Grid::new("hotkeys-full")
            .striped(true)
            .spacing(Vec2::new(24.0, 6.0))
            .show(ui, |ui| {
                for (spec, action) in configured_actions(&app.config.hotkeys) {
                    ui.label(RichText::new(action.label()).color(TEXT).size(12.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        kbd(ui, spec);
                    });
                    ui.end_row();
                }
            });
        ui.add_space(10.0);
        ui.label(
            RichText::new("Overlay-focused keys")
                .size(12.0)
                .strong()
                .color(TEXT),
        );
        ui.add_space(2.0);
        egui::Grid::new("hotkeys-local")
            .striped(true)
            .spacing(Vec2::new(24.0, 6.0))
            .show(ui, |ui| {
                for (spec, action) in local_actions(&app.config.hotkeys) {
                    ui.label(RichText::new(action.label()).color(TEXT).size(12.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        kbd(ui, spec);
                    });
                    ui.end_row();
                }
            });
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "These only fire while the overlay is focused (or its popover is hovered) and the \
                 keyboard is not in a text field — a bare `S` cannot be registered globally \
                 without stealing it from every other app.",
            )
            .color(DIM)
            .size(11.0),
        );
    });
}
fn field_row(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(DIM).size(12.0));
        ui.add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(f32::INFINITY),
        );
    });
}

// --- small widgets ------------------------------------------------------------------

fn section_header(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(13.0).color(DIM));
        ui.label(RichText::new(title).size(11.0).strong().color(DIM));
    });
    ui.add_space(6.0);
}

fn pill(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).strong().color(fg));
        });
}

fn kbd(ui: &mut egui::Ui, text: &str) {
    egui::Frame::NONE
        .fill(CHIP_BG)
        .corner_radius(CornerRadius::same(6))
        .stroke(Stroke::new(1.0, BORDER))
        .inner_margin(Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).monospace().color(TEXT));
        });
}

/// One rounded panel with a border.
fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(CARD_BG)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .stroke(Stroke::new(1.0, BORDER))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
    ui.add_space(12.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Validator};

    /// Headless render smoke test: the cards that expose the config-only knobs must lay out
    /// and paint with no window (they are the only GUI path to those settings).
    #[test]
    fn settings_and_anki_pages_render_headlessly() {
        let ctx = egui::Context::default();
        let _ = crate::gui::fonts::install_cjk_fonts(&ctx);
        let mut app = crate::app::App::new(
            Config::default(),
            None,
            Vec::new(),
            crate::gui::fonts::FontSource::Embedded,
            ctx.clone(),
        );
        // A queued note so the offline-queue card renders its populated branch, and a
        // corrupt entry so the unreadable-file warning is part of the smoke test.
        let dir = std::env::temp_dir().join(format!("ml-dash-{}", std::process::id()));
        let queue_dir = dir.join("queue");
        std::fs::create_dir_all(&queue_dir).expect("queue dir");
        crate::anki::queue::OfflineQueue::new(queue_dir.clone())
            .push(&crate::anki::note::PendingNote {
                deck: "Japanese::Mining".to_owned(),
                model: "Japanese Mining (medialingual)".to_owned(),
                tags: vec!["medialingual".to_owned()],
                fields: std::collections::BTreeMap::from([(
                    "Expression".to_owned(),
                    "食べる".to_owned(),
                )]),
                media: vec![],
            })
            .expect("push");
        std::fs::write(queue_dir.join("0000000000000000000-broken.json"), b"{nope")
            .expect("write broken entry");
        app.queue_dir = queue_dir;

        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            control_room(&mut app, ui);
            settings_page(&mut app, ui);
            anki_page(&mut app, ui);
            hotkeys_page(&app, ui);
        });
        output.textures_delta.clear();
        assert!(
            !output.shapes.is_empty(),
            "the control room, settings, anki and hotkeys pages painted nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_ages_read_as_seconds_minutes_hours() {
        use std::time::Duration;
        assert_eq!(short_duration(Duration::from_secs(0)), "0s");
        assert_eq!(short_duration(Duration::from_secs(59)), "59s");
        assert_eq!(short_duration(Duration::from_secs(60)), "1m");
        assert_eq!(short_duration(Duration::from_secs(3_599)), "59m");
        assert_eq!(short_duration(Duration::from_secs(3_600)), "1h");
        assert_eq!(short_duration(Duration::from_secs(86_400)), "24h");
    }

    /// A settings field and how to set it to one of its bounds.
    type BoundCase = (&'static str, fn(&mut Config, bool));

    /// Every range the settings card exposes must validate at both ends, or Save refuses a
    /// config the UI itself produced (the offset-step regression). Cheap guard for a whole
    /// class of bug, kept next to the ranges it protects.
    #[test]
    fn settings_ranges_stay_inside_the_validator() {
        let mutate: [BoundCase; 7] = [
            ("subtitle.font_size", |c, high| {
                c.subtitle.font_size = if high {
                    *ranges::FONT_SIZE.end()
                } else {
                    *ranges::FONT_SIZE.start()
                }
            }),
            ("subtitle.offset_step_ms", |c, high| {
                c.subtitle.offset_step_ms = if high {
                    *ranges::OFFSET_STEP_MS.end()
                } else {
                    *ranges::OFFSET_STEP_MS.start()
                }
            }),
            ("audio.buffer_seconds", |c, high| {
                c.audio.buffer_seconds = if high {
                    *ranges::BUFFER_SECONDS.end()
                } else {
                    *ranges::BUFFER_SECONDS.start()
                }
            }),
            ("audio.padding_ms", |c, high| {
                c.audio.padding_ms = if high {
                    *ranges::PADDING_MS.end()
                } else {
                    *ranges::PADDING_MS.start()
                }
            }),
            ("audio.mp3_bitrate_kbps", |c, high| {
                c.audio.mp3_bitrate_kbps = if high {
                    *ranges::BITRATE_KBPS.end()
                } else {
                    *ranges::BITRATE_KBPS.start()
                }
            }),
            ("clock.live_cue_delay_ms", |c, high| {
                c.clock.live_cue_delay_ms = if high {
                    *ranges::LIVE_DELAY_MS.end()
                } else {
                    *ranges::LIVE_DELAY_MS.start()
                }
            }),
            ("anki.retry_interval_s", |c, high| {
                c.anki.retry_interval_s = if high {
                    *ranges::RETRY_INTERVAL_S.end()
                } else {
                    *ranges::RETRY_INTERVAL_S.start()
                }
            }),
        ];

        for (field, apply) in mutate {
            for high in [false, true] {
                let mut config = Config::default();
                apply(&mut config, high);
                let mut validator = Validator::default();
                validator.check(&config);
                assert!(
                    validator.issues.is_empty(),
                    "{field} at its {} bound must stay valid, got {:?}",
                    if high { "upper" } else { "lower" },
                    validator.issues
                );
            }
        }
    }
}
