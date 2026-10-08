//! The transparent overlay window rendering: subtitle tokens, hover highlights, the
//! dictionary popover, status strip and toasts.
//!
//! Hit-testing (M4): the window is click-through by default. Each frame the UI lays the
//! subtitle out into one galley, derives a rectangle (or several, when a token wraps) per
//! token from the shaped glyph run, and records the union of interactive rectangles. The
//! [`crate::app::App`] then compares the *global* cursor (queried from the OS, since a
//! click-through window sees no mouse events) against those rectangles and toggles the
//! viewport's mouse passthrough accordingly. See `AGENTS.md` for the mechanism.

use std::sync::Arc;
use std::time::Instant;

use egui::{Color32, FontId, Painter, Pos2, Rect, Vec2};

use crate::app::App;
use crate::config::Config;
use crate::gui::popover;
use crate::tokenize::Token;

const MARGIN: f32 = 16.0;

/// One token's clickable geometry. `rects` has one entry per text row the token spans.
pub struct TokenHit {
    pub index: usize,
    pub rects: Vec<Rect>,
}

/// The laid-out subtitle for this frame: the shaped galley, its screen origin, and the
/// per-token rectangles derived from it.
struct TokenLayout {
    galley: Arc<egui::Galley>,
    origin: Pos2,
    hits: Vec<TokenHit>,
}

impl TokenLayout {
    /// The token under `pos`, if any.
    fn hit_test(&self, pos: Pos2) -> Option<usize> {
        self.hits
            .iter()
            .find(|hit| hit.rects.iter().any(|rect| rect.contains(pos)))
            .map(|hit| hit.index)
    }
}

/// Render the overlay contents into the eframe root UI.
pub fn show(app: &mut App, ui: &mut egui::Ui) {
    app.drain_config_issues();
    app.toasts.expire(Instant::now());

    // Hidden (Ctrl+Alt+H): draw nothing and drop hover/pin state so it never reappears
    // stale; the window stays click-through so the video underneath is not blocked.
    if !app.visible {
        app.interactive_rects = Vec::new();
        app.hover_token = None;
        app.popover = None;
        return;
    }

    let area = ui.max_rect();
    let text = app.subtitle_text();
    let tokens = app.tokens_cached(&text);
    let layout = build_layout(&app.config, ui, area, &text, &tokens);
    tracing::trace!(
        area = ?area,
        origin = ?layout.origin,
        rows = layout.galley.rows.len(),
        hits = layout.hits.len(),
        "overlay layout"
    );

    // Hover is driven by the OS cursor (available even while click-through).
    let cursor = app.cursor_local;
    let hover = cursor.and_then(|pos| layout.hit_test(pos));
    app.hover_token = hover;

    let (clicked, escape) = ui.input(|input| {
        (
            input.pointer.primary_clicked(),
            input.key_pressed(egui::Key::Escape),
        )
    });
    app.update_popover(hover, cursor, clicked, escape, &tokens);

    if app.config.window.backing_box {
        paint_backing_box(ui, area, &layout.galley);
    }
    paint_token_highlights(app, ui, &layout);
    paint_outlined_text(ui, layout.origin, &layout.galley);

    // A manual clock that hasn't started yet gets a visible invitation — it was invisible
    // friction that the clock sits paused at zero until Ctrl+Alt+Space.
    if app.waiting_for_start() {
        paint_waiting_hint(app, ui, area, layout.origin.y);
    }

    let popover_rect = paint_popover(app, ui, &layout, area);

    // Interactive rectangles: every token plus the popover (when open). The status strip and
    // the empty overlay remain click-through.
    let mut interactive: Vec<Rect> = layout
        .hits
        .iter()
        .flat_map(|hit| hit.rects.iter().copied())
        .collect();
    if let Some(rect) = popover_rect {
        interactive.push(rect);
    }
    app.interactive_rects = interactive;

    if app.show_status {
        paint_status_strip(app, ui, area);
    }
    paint_toasts(app, ui, area);
    handle_quit(ui);
}

/// Dim invitation shown while a manual clock is still at zero with subtitles loaded, so
/// the "press the clock hotkey when the video starts" step is visible instead of magical.
fn paint_waiting_hint(app: &App, ui: &egui::Ui, area: Rect, sub_top: f32) {
    let dim = Color32::from_rgb(148, 163, 184);
    let label = format!(
        "waiting for video — press {} when it starts",
        app.config.hotkeys.clock_start_pause
    );
    let hint = ui.fonts_mut(|f| {
        f.layout(
            label,
            FontId::proportional(16.0),
            dim,
            f32::INFINITY, // no wrap: one line
        )
    });
    let x = (area.center().x - hint.size().x / 2.0).max(area.left());
    // Above the active subtitle; otherwise pinned near the bottom edge like a real sub.
    let y = if app.active_cue.is_some() {
        (sub_top - hint.size().y - 8.0).max(area.top())
    } else {
        area.bottom() - MARGIN - hint.size().y
    };
    ui.painter().galley(Pos2::new(x, y), hint, dim);
}

fn build_layout(
    config: &Config,
    ui: &mut egui::Ui,
    area: Rect,
    text: &str,
    tokens: &[Token],
) -> TokenLayout {
    let galley = layout_subtitle(config, ui, area, text);
    let origin = subtitle_position(config, area, &galley);
    let hits = token_hits(&galley, origin, text, tokens);
    TokenLayout {
        galley,
        origin,
        hits,
    }
}

fn layout_subtitle(
    config: &Config,
    ui: &mut egui::Ui,
    area: Rect,
    text: &str,
) -> Arc<egui::Galley> {
    let font_size = config.subtitle.font_size;
    let wrap_width = (area.width() - MARGIN * 2.0).max(50.0);
    ui.fonts_mut(|fonts| {
        fonts.layout(
            text.to_owned(),
            FontId::proportional(font_size),
            Color32::WHITE,
            wrap_width,
        )
    })
}

fn subtitle_position(config: &Config, area: Rect, galley: &Arc<egui::Galley>) -> Pos2 {
    let x = area.center().x - galley.rect.width() / 2.0;
    let y = match config.subtitle.position.as_str() {
        "top_center" => area.top() + MARGIN,
        "custom" => {
            area.top() + area.height() * config.subtitle.custom_y - galley.rect.height() / 2.0
        }
        // "bottom_center" and anything else validated to it
        _ => area.bottom() - MARGIN - galley.rect.height(),
    };
    Pos2::new(x, y)
}

/// Derive per-token rectangles from the shaped glyph run.
///
/// The galley stores one [`egui::epaint::text::Glyph`] per character (newlines omitted), so
/// walking rows and glyphs in lockstep recovers a character-indexed list of boxes. Token byte
/// offsets are then mapped to character ranges and the boxes of each token are merged per row
/// (a token can wrap across two rows).
fn token_hits(galley: &egui::Galley, origin: Pos2, text: &str, tokens: &[Token]) -> Vec<TokenHit> {
    let char_count = text.chars().count();
    let mut char_boxes: Vec<Option<(usize, Rect)>> = vec![None; char_count];
    let mut char_index = 0usize;
    for (row_index, row) in galley.rows.iter().enumerate() {
        for glyph in &row.glyphs {
            if char_index >= char_boxes.len() {
                break;
            }
            let top_left = origin
                + Vec2::new(
                    row.pos.x + glyph.pos.x,
                    row.pos.y + glyph.pos.y - glyph.font_ascent,
                );
            char_boxes[char_index] = Some((
                row_index,
                Rect::from_min_size(top_left, Vec2::new(glyph.advance_width, glyph.line_height)),
            ));
            char_index += 1;
        }
        if row.ends_with_newline {
            char_index += 1; // the '\n' itself is omitted from the glyph run
        }
    }

    tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            let start = char_index_at(text, token.byte_start);
            let end = char_index_at(text, token.byte_end).max(start);
            let mut rects: Vec<Rect> = Vec::new();
            let mut current_row: Option<usize> = None;
            let mut current: Option<Rect> = None;
            for char_pos in start..end {
                let Some(Some((row, rect))) = char_boxes.get(char_pos).copied() else {
                    continue;
                };
                if current_row == Some(row) {
                    if let Some(union) = &mut current {
                        *union = union.union(rect);
                    }
                } else {
                    if let Some(done) = current.take() {
                        rects.push(done);
                    }
                    current = Some(rect);
                    current_row = Some(row);
                }
            }
            if let Some(done) = current {
                rects.push(done);
            }
            TokenHit { index, rects }
        })
        .collect()
}

/// Character index of a byte offset, or 0 if the offset is not a char boundary.
fn char_index_at(text: &str, byte: usize) -> usize {
    text.get(..byte).map_or(0, |prefix| prefix.chars().count())
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

fn paint_token_highlights(app: &App, ui: &egui::Ui, layout: &TokenLayout) {
    let selected = app.popover.as_ref().map(|popover| popover.token_index);
    let painter = ui.painter();
    if let Some(index) = app.hover_token {
        if Some(index) != selected {
            paint_highlight(
                painter,
                layout,
                index,
                Color32::from_rgba_unmultiplied(130, 180, 255, 45),
            );
        }
    }
    if let Some(index) = selected {
        paint_highlight(
            painter,
            layout,
            index,
            Color32::from_rgba_unmultiplied(130, 180, 255, 100),
        );
    }
}

fn paint_highlight(painter: &Painter, layout: &TokenLayout, index: usize, color: Color32) {
    if let Some(hit) = layout.hits.get(index) {
        for rect in &hit.rects {
            painter.rect_filled(*rect, 3.0, color);
        }
    }
}

/// Draw the laid-out galley with a real 8-direction outline: the same galley is painted
/// eight times at ±1px offsets with the text color overridden to black, then once more in
/// white on top. It is the actual shaped text, not a per-character estimate.
fn paint_outlined_text(ui: &mut egui::Ui, text_pos: Pos2, galley: &Arc<egui::Galley>) {
    if galley.rows.is_empty() {
        return;
    }
    let painter = ui.painter();
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
            text_pos + Vec2::new(dx, dy),
            Arc::clone(galley),
            Color32::BLACK,
        );
    }
    painter.galley(text_pos, Arc::clone(galley), Color32::WHITE);
}

/// Paint the open popover and record its rectangle for the next frame's hit-testing.
fn paint_popover(
    app: &mut App,
    ui: &mut egui::Ui,
    layout: &TokenLayout,
    area: Rect,
) -> Option<Rect> {
    let state = app.popover.as_ref()?;
    let anchor = layout
        .hits
        .get(state.token_index)
        .and_then(|hit| hit.rects.first().copied())?;
    let rect = popover::paint(ui, &state.data, anchor, area);
    if let Some(state) = app.popover.as_mut() {
        state.rect = Some(rect);
    }
    Some(rect)
}

fn paint_status_strip(app: &App, ui: &mut egui::Ui, area: Rect) {
    let dictionary = if app.dictionary.is_some() {
        "dict ready"
    } else {
        "dict missing"
    };
    let audio = match &app.audio {
        Some(capture) => match capture.ring() {
            Some(ring) => format!("audio {:.1}s buf", ring.buffered_seconds()),
            None => "audio retrying…".to_owned(),
        },
        None => "audio off".to_owned(),
    };
    let text = format!(
        "MediaLingual · {dictionary} · {audio} · offset {:+} ms · {}",
        app.subtitle_offset_ms(),
        if app.hit_testing {
            "interactive"
        } else {
            "click-through"
        },
    );
    let galley = ui.fonts_mut(|fonts| {
        fonts.layout_no_wrap(text, FontId::proportional(13.0), Color32::from_gray(210))
    });
    let rect = Rect::from_min_size(
        Pos2::new(area.left() + 8.0, area.top() + 6.0),
        galley.rect.size() + Vec2::new(10.0, 4.0),
    );
    ui.painter()
        .rect_filled(rect, 4.0, Color32::from_black_alpha(120));
    ui.painter()
        .galley(rect.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
}

fn paint_toasts(app: &App, ui: &mut egui::Ui, area: Rect) {
    let mut y = area.top() + 34.0;
    for toast in app.toasts.iter() {
        let galley = ui.fonts_mut(|fonts| {
            fonts.layout(
                toast.message.clone(),
                FontId::proportional(14.0),
                Color32::WHITE,
                (area.width() - MARGIN * 2.0).max(80.0),
            )
        });
        let rect = Rect::from_min_size(
            Pos2::new(area.left() + MARGIN, y),
            galley.rect.size() + Vec2::new(14.0, 8.0),
        );
        let color = if toast.is_error {
            Color32::from_rgba_unmultiplied(150, 40, 40, 230)
        } else {
            Color32::from_rgba_unmultiplied(30, 70, 120, 230)
        };
        ui.painter().rect_filled(rect, 5.0, color);
        ui.painter()
            .galley(rect.min + Vec2::new(7.0, 4.0), galley, Color32::WHITE);
        y += rect.height() + 4.0;
    }
}

fn handle_quit(ui: &mut egui::Ui) {
    if ui.input(|input| input.key_pressed(egui::Key::Q) && input.modifiers.ctrl) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::JapaneseTokenizer;

    #[test]
    fn char_index_maps_byte_offsets() {
        let text = "猫が";
        assert_eq!(char_index_at(text, 0), 0);
        assert_eq!(char_index_at(text, "猫".len()), 1);
        assert_eq!(char_index_at(text, "猫が".len()), 2);
        // Non-boundary offsets fall back to 0 instead of panicking.
        assert_eq!(char_index_at(text, 1), 0);
    }

    #[test]
    fn token_hits_cover_every_token_in_source_order() {
        // A pure-geometry-ish check without a GPU: lay the text out through egui's font
        // system and confirm each token gets at least one rectangle in reading order.
        let ctx = egui::Context::default();
        let tokens = JapaneseTokenizer::new()
            .expect("tokenizer")
            .tokenize("私は学生です。")
            .expect("tokenize");
        let mut hits: Option<Vec<TokenHit>> = None;
        let mut output = ctx.run_ui(Default::default(), |ui| {
            let area = ui.max_rect();
            let galley = layout_subtitle(
                &crate::config::Config::default(),
                ui,
                area,
                "私は学生です。",
            );
            let origin = subtitle_position(&crate::config::Config::default(), area, &galley);
            hits = Some(token_hits(&galley, origin, "私は学生です。", &tokens));
        });
        // The font atlas delta must be acknowledged or epaint panics on drop.
        output.textures_delta.clear();
        let hits = hits.expect("context ran");
        assert_eq!(hits.len(), tokens.len());
        assert!(hits.iter().all(|hit| !hit.rects.is_empty()));
        // Rectangles are monotonic left-to-right for a single unwrapped line.
        let lefts: Vec<f32> = hits.iter().map(|hit| hit.rects[0].left()).collect();
        assert!(lefts.windows(2).all(|pair| pair[0] <= pair[1]), "{lefts:?}");
    }
}
