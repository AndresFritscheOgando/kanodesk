//! Small drawing helpers shared by the windows, so buttons, cards and
//! status lines look the same everywhere (15 §2). Colours come from
//! [`crate::theme`]; every helper returns egui's response so callers keep
//! using `.clicked()` and `.on_hover_text()`.

use egui::{Button, Color32, CornerRadius, Frame, Margin, Response, RichText, Stroke, Ui};

use crate::main_model::Level;
use crate::theme::{CARD_RADIUS, palette};

/// How long "Copied ✔" stays on a copy button, in seconds.
pub const COPIED_FOR: f64 = 1.5;

/// The main action of a card or dialog: filled with the accent colour.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.add(Button::new(RichText::new(text).color(p.on_accent)).fill(p.accent))
}

/// A destructive action (Deny, Disconnect, Remove): filled red.
pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.add(Button::new(RichText::new(text).color(p.on_accent)).fill(p.danger))
}

/// A low-weight action without a frame (✕, Open Settings).
pub fn quiet_button(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    ui.add(Button::new(text).frame(false))
}

/// A frame around a group of related content, as wide as the space given.
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let p = palette(ui);
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

/// A card or section title: heading size in the strong text colour.
pub fn section_title(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.label(RichText::new(text).heading().color(p.text_strong))
}

/// A small caption above a value ("Your ID").
pub fn field_label(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.label(RichText::new(text).small().color(p.text_weak))
}

/// The colour that stands for `level`.
pub fn level_color(ui: &Ui, level: Level) -> Color32 {
    let p = palette(ui);
    match level {
        Level::Ok => p.success,
        Level::Busy => p.warn,
        Level::Problem => p.danger_text,
        Level::Idle => p.text_weak,
    }
}

/// A coloured dot and a short label: "● Online".
pub fn status_dot(ui: &mut Ui, level: Level, text: &str) -> Response {
    let color = level_color(ui, level);
    // Inside a right-to-left row the label goes first, so the dot still
    // reads before it.
    let reversed = ui.layout().prefer_right_to_left();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let label = |ui: &mut Ui| ui.label(RichText::new(text).color(color));
        if reversed {
            label(ui);
        }
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        if !reversed {
            label(ui);
        }
    })
    .response
}

/// A small outlined label in the level's colour ("Relayed", "30 fps").
pub fn chip(ui: &mut Ui, level: Level, text: &str) -> Response {
    let color = level_color(ui, level);
    let border = if level == Level::Idle {
        palette(ui).border
    } else {
        color.gamma_multiply(0.6)
    };
    Frame::new()
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).small().color(color)))
        .response
}

/// A full-width tinted strip for something that needs attention.
pub fn banner<R>(ui: &mut Ui, level: Level, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let color = level_color(ui, level);
    Frame::new()
        .fill(color.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(CornerRadius::same(crate::theme::WIDGET_RADIUS))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

/// A banner holding one line of text in the level's colour.
pub fn banner_text(ui: &mut Ui, level: Level, text: &str) {
    let color = level_color(ui, level);
    banner(ui, level, |ui| {
        ui.label(RichText::new(text).color(color));
    });
}

/// Gives an icon-only button ("⋯", "✕") a name screen readers can say.
pub fn name_button(response: Response, name: &str) -> Response {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
    response
}

/// The copy button's label `now` (seconds, egui time), given when it was
/// last clicked.
pub fn copied_label(copied_at: Option<f64>, now: f64) -> &'static str {
    match copied_at {
        Some(t) if now >= t && now - t < COPIED_FOR => "Copied ✔",
        _ => "Copy",
    }
}

/// "Copy", which puts `value` on the clipboard and says "Copied ✔" for a
/// moment. `id_salt` tells several copy buttons in one place apart.
pub fn copy_button(ui: &mut Ui, id_salt: &str, value: &str) -> Response {
    let id = ui.id().with(("copy", id_salt));
    let now = ui.input(|i| i.time);
    let copied_at: Option<f64> = ui.data(|d| d.get_temp(id));
    let label = copied_label(copied_at, now);
    let copied = label != "Copy";
    let text = if copied {
        RichText::new(label).color(palette(ui).success)
    } else {
        RichText::new(label)
    };
    let response = ui.add(Button::new(text).min_size(egui::vec2(76.0, 0.0)));
    if response.clicked() {
        ui.ctx().copy_text(value.to_owned());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if copied {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_feedback_lasts_a_moment() {
        assert_eq!(copied_label(None, 10.0), "Copy");
        assert_eq!(copied_label(Some(10.0), 10.0), "Copied ✔");
        assert_eq!(copied_label(Some(10.0), 11.0), "Copied ✔");
        assert_eq!(copied_label(Some(10.0), 10.0 + COPIED_FOR), "Copy");
        assert_eq!(copied_label(Some(10.0), 12.0), "Copy");
        assert_eq!(copied_label(Some(10.0), 9.0), "Copy", "clock went back");
    }
}
