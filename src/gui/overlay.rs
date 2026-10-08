//! The transparent overlay window rendering: subtitle text, backing box, status strip,
//! toasts. Hit-testing/popover/furigana arrive with M4; for M1/M2 this renders the
//! active cue (or the demo string) with real outlined text through egui's layout pipeline.

use std::sync::Arc;
use std::time::Instant;

use egui::{Color32, FontId, Pos2, Rect, Vec2};

use crate::app::App;

const MARGIN: f32 = 16.0;

/// Render the overlay contents into the eframe root UI.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    app.drain_config_issues();
    let now = Instant::now();
    app.toasts.expire(now);

    let area = ui.max_rect();
    let galley = layout_subtitle(app, ui, area);

    if app.config.window.backing_box {
        paint_backing_box(ui, area, &galley);
    }
    let text_pos = subtitle_position(app, area, &galley);
    paint_outlined_text(ui, text_pos, &galley);

    paint_status_strip(app, ui, area);
    paint_toasts(app, ui, area);
    handle_quit(ui);
}

fn layout_subtitle(app: &App, ui: &mut egui::Ui, area: Rect) -> Arc<egui::Galley> {
    let font_size = app.config.subtitle.font_size;
    let wrap_width = (area.width() - MARGIN * 2.0).max(50.0);
    let text = app.subtitle_text();
    ui.fonts_mut(|fonts| {
        fonts.layout(
            text,
            FontId::proportional(font_size),
            Color32::WHITE,
            wrap_width,
        )
    })
}

fn subtitle_position(app: &App, area: Rect, galley: &Arc<egui::Galley>) -> Pos2 {
    let x = area.center().x - galley.rect.width() / 2.0;
    let y = match app.config.subtitle.position.as_str() {
        "top_center" => area.top() + MARGIN,
        "custom" => {
            let frac = app.config.subtitle.custom_y.clamp(0.0, 1.0);
            area.top() + area.height() * frac - galley.rect.height() / 2.0
        }
        // "bottom_center" and anything else validated to it
        _ => area.bottom() - MARGIN - galley.rect.height(),
    };
    Pos2::new(x, y)
}

fn paint_backing_box(ui: &mut egui::Ui, area: Rect, galley: &Arc<egui::Galley>) {
    let rect = Rect::from_center_size(
        Pos2::new(area.center().x, galley.rect.center().y),
        Vec2::new(
            (galley.rect.width() + MARGIN).min(area.width() - 8.0),
            galley.rect.height() + MARGIN * 0.6,
        ),
    );
    ui.painter()
        .rect_filled(rect, 6.0, Color32::from_black_alpha(140));
}

/// Draw the laid-out galley with a real 8-direction outline: the same galley is painted
/// eight times at ±1px offsets with the text color overridden to black, then once more in
/// white on top. It is the actual shaped text, not a per-character estimate.
fn paint_outlined_text(ui: &mut egui::Ui, pos: Pos2, galley: &Arc<egui::Galley>) {
    let painter = ui.painter();
    if galley.rows.is_empty() {
        return;
    }
    let offset = 1.0;
    for (dx, dy) in [
        (-offset, 0.0),
        (offset, 0.0),
        (0.0, -offset),
        (0.0, offset),
        (-offset, -offset),
        (-offset, offset),
        (offset, -offset),
        (offset, offset),
    ] {
        painter.galley_with_override_text_color(
            pos + Vec2::new(dx, dy),
            Arc::clone(galley),
            Color32::BLACK,
        );
    }
    painter.galley(pos, Arc::clone(galley), Color32::WHITE);
}

fn paint_status_strip(app: &App, ui: &mut egui::Ui, area: Rect) {
    let status = format!(
        "MediaLingual | font: {} | clock: {} | offset: {:+} ms",
        app.font_source,
        clock_label(&app.config.clock.source),
        app.subtitle_offset_ms(),
    );
    let galley = ui.fonts_mut(|fonts| {
        fonts.layout_no_wrap(status, FontId::monospace(11.0), Color32::from_gray(200))
    });
    ui.painter().galley(
        Pos2::new(area.left() + 6.0, area.top() + 4.0),
        galley,
        Color32::GRAY,
    );
}

fn clock_label(source: &crate::config::ClockSource) -> &'static str {
    match source {
        crate::config::ClockSource::Manual => "manual",
        crate::config::ClockSource::MpvIpc => "mpv-ipc",
        crate::config::ClockSource::Mpris => "mpris",
        crate::config::ClockSource::WhisperLive => "whisper-live",
    }
}

fn paint_toasts(app: &App, ui: &mut egui::Ui, area: Rect) {
    let mut y = area.top() + 24.0;
    for toast in app.toasts.iter() {
        let color = if toast.is_error {
            Color32::from_rgba_unmultiplied(120, 20, 20, 225)
        } else {
            Color32::from_rgba_unmultiplied(20, 60, 20, 225)
        };
        let galley = ui.fonts_mut(|fonts| {
            fonts.layout(
                toast.message.clone(),
                FontId::proportional(13.0),
                Color32::WHITE,
                320.0,
            )
        });
        let rect = Rect::from_min_size(
            Pos2::new(area.right() - galley.rect.width() - 20.0, y),
            Vec2::new(galley.rect.width() + 12.0, galley.rect.height() + 8.0),
        );
        ui.painter().rect_filled(rect, 4.0, color);
        ui.painter()
            .galley(rect.min + Vec2::new(6.0, 4.0), galley, Color32::WHITE);
        y += rect.height() + 6.0;
    }
}

/// Cmd/Ctrl+Q quits while the overlay has focus (the window is undecorated, so there is
/// no close button).
fn handle_quit(ui: &mut egui::Ui) {
    let quit = ui.input(|i| i.key_pressed(egui::Key::Q) && i.modifiers.command);
    if quit {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }
}
