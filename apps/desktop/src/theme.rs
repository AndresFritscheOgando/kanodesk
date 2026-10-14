//! The app's look, shared by the main window, the connection manager and
//! the viewer (15 §2): colour tokens for a dark and a light theme, text
//! sizes, spacing and corner radii. [`install`] puts them on an egui
//! context; the system's light/dark setting picks the theme.
//!
//! Every text colour pair is checked against WCAG AA in the tests, so a
//! new value that fails is caught here rather than on screen.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Theme, Vec2};

/// Colour tokens for one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window background.
    pub bg: Color32,
    /// Cards and panels.
    pub surface: Color32,
    /// Popups, menus and dialogs.
    pub surface_raised: Color32,
    /// Card and field outlines, separators.
    pub border: Color32,
    /// Normal text.
    pub text: Color32,
    /// Headings, values and other text that should stand out.
    pub text_strong: Color32,
    /// Secondary text: hints, captions.
    pub text_weak: Color32,
    /// Filled primary buttons, selection.
    pub accent: Color32,
    /// Accent-coloured text and outlines.
    pub accent_text: Color32,
    /// Text on [`Palette::accent`] and [`Palette::danger`] fills.
    pub on_accent: Color32,
    /// Good news: online, saved, copied.
    pub success: Color32,
    /// Needs attention: retrying, sessions connected.
    pub warn: Color32,
    /// Filled buttons for destructive actions.
    pub danger: Color32,
    /// Error text.
    pub danger_text: Color32,
    /// Plain button background.
    pub button: Color32,
    /// Plain button background under the pointer.
    pub button_hover: Color32,
}

/// The dark theme.
pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x14, 0x16, 0x18),
    surface: Color32::from_rgb(0x1C, 0x1F, 0x22),
    surface_raised: Color32::from_rgb(0x24, 0x28, 0x2C),
    border: Color32::from_rgb(0x2E, 0x33, 0x38),
    text: Color32::from_rgb(0xD5, 0xD9, 0xDE),
    text_strong: Color32::from_rgb(0xF3, 0xF5, 0xF7),
    text_weak: Color32::from_rgb(0x9A, 0xA3, 0xAD),
    accent: Color32::from_rgb(0x1F, 0x7A, 0x70),
    accent_text: Color32::from_rgb(0x4F, 0xD1, 0xC1),
    on_accent: Color32::WHITE,
    success: Color32::from_rgb(0x3F, 0xB9, 0x50),
    warn: Color32::from_rgb(0xF0, 0xA6, 0x3A),
    danger: Color32::from_rgb(0xC9, 0x30, 0x35),
    danger_text: Color32::from_rgb(0xFF, 0x7B, 0x72),
    button: Color32::from_rgb(0x2C, 0x31, 0x36),
    button_hover: Color32::from_rgb(0x36, 0x3C, 0x42),
};

/// The light theme.
pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF6, 0xF7, 0xF8),
    surface: Color32::WHITE,
    surface_raised: Color32::WHITE,
    border: Color32::from_rgb(0xD9, 0xDE, 0xE3),
    text: Color32::from_rgb(0x2A, 0x2F, 0x35),
    text_strong: Color32::from_rgb(0x10, 0x13, 0x16),
    text_weak: Color32::from_rgb(0x5B, 0x66, 0x70),
    accent: Color32::from_rgb(0x1F, 0x7A, 0x70),
    accent_text: Color32::from_rgb(0x17, 0x66, 0x5E),
    on_accent: Color32::WHITE,
    success: Color32::from_rgb(0x1A, 0x7F, 0x37),
    warn: Color32::from_rgb(0x9A, 0x67, 0x00),
    danger: Color32::from_rgb(0xC9, 0x30, 0x35),
    danger_text: Color32::from_rgb(0xB4, 0x23, 0x18),
    button: Color32::from_rgb(0xE9, 0xEC, 0xEF),
    button_hover: Color32::from_rgb(0xDD, 0xE1, 0xE5),
};

/// The device ID's text style (large, monospace).
pub fn display() -> TextStyle {
    TextStyle::Name("display".into())
}

/// The one-time password's text style (a step below [`display`]).
pub fn display_small() -> TextStyle {
    TextStyle::Name("display_small".into())
}

/// Inner margin of single-line text fields, so they are as tall as buttons.
pub const FIELD_MARGIN: Vec2 = Vec2::new(8.0, 5.0);

/// Corner radius of cards and dialogs.
pub const CARD_RADIUS: u8 = 10;
/// Corner radius of buttons and fields.
pub const WIDGET_RADIUS: u8 = 6;

/// Installs both themes on `ctx`; the system setting picks one.
pub fn install(ctx: &egui::Context) {
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System);
    ctx.set_style_of(Theme::Dark, style(&DARK, true));
    ctx.set_style_of(Theme::Light, style(&LIGHT, false));
}

/// The palette `ui` draws with.
pub fn palette(ui: &egui::Ui) -> &'static Palette {
    palette_for(ui.visuals().dark_mode)
}

/// The palette of the dark (`true`) or light theme.
pub fn palette_for(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// A full style for one palette.
fn style(p: &Palette, dark: bool) -> egui::Style {
    let mut style = egui::Style::default();
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let widget_radius = CornerRadius::same(WIDGET_RADIUS);
    v.panel_fill = p.bg;
    v.window_fill = p.surface_raised;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.menu_corner_radius = widget_radius;
    v.extreme_bg_color = p.bg;
    v.text_edit_bg_color = Some(if dark { p.bg } else { p.surface });
    v.faint_bg_color = p.surface;
    v.code_bg_color = p.surface_raised;
    v.hyperlink_color = p.accent_text;
    v.error_fg_color = p.danger_text;
    v.warn_fg_color = p.warn;
    v.weak_text_color = Some(p.text_weak);
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, p.on_accent);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = widget_radius;
    w.inactive.bg_fill = p.button;
    w.inactive.weak_bg_fill = p.button;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text_strong);
    w.inactive.corner_radius = widget_radius;
    w.hovered.bg_fill = p.button_hover;
    w.hovered.weak_bg_fill = p.button_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent_text);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text_strong);
    w.hovered.corner_radius = widget_radius;
    w.active.bg_fill = p.button_hover;
    w.active.weak_bg_fill = p.button_hover;
    w.active.bg_stroke = Stroke::new(1.0, p.accent_text);
    w.active.fg_stroke = Stroke::new(2.0, p.text_strong);
    w.active.corner_radius = widget_radius;
    w.open.bg_fill = p.surface_raised;
    w.open.weak_bg_fill = p.button_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border);
    w.open.fg_stroke = Stroke::new(1.0, p.text_strong);
    w.open.corner_radius = widget_radius;
    style.visuals = v;

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 8.0);
    s.button_padding = Vec2::new(12.0, 5.0);
    s.interact_size.y = 28.0;
    s.window_margin = egui::Margin::same(16);
    // Popups, menus and dialogs (egui frames modals like popups).
    s.menu_margin = egui::Margin::same(14);
    s.combo_width = 140.0;

    let proportional = |size| FontId::new(size, FontFamily::Proportional);
    let monospace = |size| FontId::new(size, FontFamily::Monospace);
    style.text_styles = [
        (TextStyle::Small, proportional(12.0)),
        (TextStyle::Body, proportional(14.0)),
        (TextStyle::Button, proportional(14.0)),
        (TextStyle::Monospace, monospace(14.0)),
        (TextStyle::Heading, proportional(19.0)),
        (display(), monospace(30.0)),
        (display_small(), monospace(24.0)),
    ]
    .into();
    style
}

/// WCAG relative luminance of an sRGB colour (alpha ignored).
fn luminance(c: Color32) -> f32 {
    let channel = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

/// WCAG contrast ratio of two colours, 1.0 to 21.0.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_bounds() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(DARK.bg, DARK.bg) - 1.0).abs() < 0.001);
        assert_eq!(
            contrast(DARK.text, DARK.bg),
            contrast(DARK.bg, DARK.text),
            "symmetric"
        );
    }

    #[test]
    fn palettes_pass_wcag_aa() {
        for (name, p) in [("dark", DARK), ("light", LIGHT)] {
            let check = |what: &str, fg: Color32, bg: Color32, min: f32| {
                let c = contrast(fg, bg);
                assert!(c >= min, "{name}: {what} is {c:.2}:1, needs {min}:1");
            };
            for (bg_name, bg) in [
                ("bg", p.bg),
                ("surface", p.surface),
                ("raised", p.surface_raised),
            ] {
                check(&format!("text on {bg_name}"), p.text, bg, 7.0);
                check(&format!("strong text on {bg_name}"), p.text_strong, bg, 7.0);
                check(&format!("weak text on {bg_name}"), p.text_weak, bg, 4.5);
                check(&format!("danger text on {bg_name}"), p.danger_text, bg, 4.5);
                check(&format!("warn on {bg_name}"), p.warn, bg, 4.5);
                check(&format!("success on {bg_name}"), p.success, bg, 4.5);
                check(&format!("accent text on {bg_name}"), p.accent_text, bg, 4.5);
            }
            check("text on accent", p.on_accent, p.accent, 4.5);
            check("text on danger", p.on_accent, p.danger, 4.5);
            check("button text", p.text_strong, p.button, 4.5);
            check("button text, hovered", p.text_strong, p.button_hover, 4.5);
        }
    }

    #[test]
    fn install_sets_both_themes() {
        let ctx = egui::Context::default();
        install(&ctx);
        for (theme, p) in [(Theme::Dark, DARK), (Theme::Light, LIGHT)] {
            let style = ctx.style_of(theme);
            assert!(style.text_styles.contains_key(&display()));
            assert!(style.text_styles.contains_key(&display_small()));
            assert_eq!(style.visuals.panel_fill, p.bg);
            assert_eq!(style.visuals.error_fg_color, p.danger_text);
            assert_eq!(style.spacing.interact_size.y, 28.0);
        }
        assert_eq!(palette_for(true), &DARK);
        assert_eq!(palette_for(false), &LIGHT);
    }
}
