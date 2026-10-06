//! Visual tokens, fonts, painted row icons, and match-highlight text jobs.
//!
//! One accent, Lantern amber, carries everything that means "this is what
//! you're looking for": matched characters, the selection edge, focus,
//! links. Everything else is neutral slate (dark) or paper (light). Text is
//! Segoe UI, Windows' own face, loaded from `C:\Windows\Fonts` (egui's
//! bundled Ubuntu Light read as foreign on Windows); egui's fonts stay as
//! fallbacks.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, Stroke, TextStyle};

use crate::model::IconKind;

/// Colors for one theme variant. Read the active one with [`palette`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window background.
    pub base: Color32,
    /// Raised areas: header row, status line, popups, inputs.
    pub raised: Color32,
    /// Hover and selected-row band.
    pub band: Color32,
    /// Hairlines between regions.
    pub rule: Color32,
    /// Outline of controls (checkboxes, buttons, fields).
    pub edge: Color32,
    pub text: Color32,
    /// Paths, captions, counts.
    pub muted: Color32,
    /// The single accent (matches, selection edge, focus, links).
    pub lantern: Color32,
    /// Matched-character text color inside names.
    pub match_fg: Color32,
    /// Matched-character background (transparent in dark mode).
    pub match_bg: Color32,
    /// Error text.
    pub error: Color32,
}

pub const DARK: Palette = Palette {
    base: Color32::from_rgb(0x1b, 0x1f, 0x24),
    raised: Color32::from_rgb(0x22, 0x27, 0x2e),
    band: Color32::from_rgb(0x2c, 0x33, 0x3c),
    rule: Color32::from_rgb(0x2f, 0x35, 0x3d),
    edge: Color32::from_rgb(0x4a, 0x52, 0x5c),
    text: Color32::from_rgb(0xe6, 0xe3, 0xdc),
    muted: Color32::from_rgb(0x8b, 0x93, 0x9c),
    lantern: Color32::from_rgb(0xf0, 0xb4, 0x4c),
    match_fg: Color32::from_rgb(0xf0, 0xb4, 0x4c),
    match_bg: Color32::TRANSPARENT,
    error: Color32::from_rgb(0xf2, 0x8b, 0x82),
};

pub const LIGHT: Palette = Palette {
    base: Color32::from_rgb(0xf6, 0xf6, 0xf3),
    raised: Color32::from_rgb(0xec, 0xec, 0xe7),
    band: Color32::from_rgb(0xe2, 0xe2, 0xdb),
    rule: Color32::from_rgb(0xd9, 0xd9, 0xd2),
    edge: Color32::from_rgb(0xb4, 0xb4, 0xab),
    text: Color32::from_rgb(0x1f, 0x23, 0x28),
    muted: Color32::from_rgb(0x6a, 0x71, 0x7a),
    lantern: Color32::from_rgb(0xa8, 0x6a, 0x00),
    match_fg: Color32::from_rgb(0x1f, 0x23, 0x28),
    match_bg: Color32::from_rgb(0xfb, 0xe3, 0xb0),
    error: Color32::from_rgb(0xb3, 0x26, 0x1e),
};

/// Palette of the theme egui is currently showing.
#[must_use]
pub fn palette(ctx: &egui::Context) -> Palette {
    if ctx.theme() == egui::Theme::Dark {
        DARK
    } else {
        LIGHT
    }
}

/// Query-line text size.
pub const QUERY_SIZE: f32 = 22.0;
/// Body text size (rows, settings).
pub const BODY_SIZE: f32 = 14.0;
/// Captions, header row, status line.
pub const SMALL_SIZE: f32 = 12.0;

/// Font family for the semibold face (query line only).
pub fn semibold() -> egui::FontFamily {
    egui::FontFamily::Name("semibold".into())
}

/// Theme choice persisted via eframe storage (`"floki-theme"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    Dark,
    Light,
    #[default]
    System,
}

impl ThemeMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
            ThemeMode::System => "system",
        }
    }

    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s {
            "dark" => ThemeMode::Dark,
            "light" => ThemeMode::Light,
            _ => ThemeMode::System,
        }
    }

    /// Menu / Settings label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::Dark => "Dark",
            ThemeMode::Light => "Light",
            ThemeMode::System => "Same as Windows",
        }
    }
}

/// Load Segoe UI (regular + semibold) from the Windows font folder ahead of
/// egui's bundled fonts. Missing files keep egui's defaults.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map_or_else(|| std::path::PathBuf::from(r"C:\Windows"), Into::into)
        .join("Fonts");
    let load = |file: &str| std::fs::read(dir.join(file)).ok();
    if let Some(bytes) = load("segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe".into(), egui::FontData::from_owned(bytes).into());
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "segoe".into());
        }
    }
    let mut bold = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = load("seguisb.ttf") {
        fonts
            .font_data
            .insert("segoe-sb".into(), egui::FontData::from_owned(bytes).into());
        bold.insert(0, "segoe-sb".into());
    }
    fonts.families.insert(semibold(), bold);
    ctx.set_fonts(fonts);
}

/// egui visuals for one palette: flat, 4 px corners, hairline borders, no
/// shadows; selection and focus in Lantern.
#[must_use]
pub fn visuals(p: Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.dark_mode = dark;
    v.panel_fill = p.base;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.raised;
    v.faint_bg_color = p.raised;
    v.code_bg_color = p.raised;
    v.text_edit_bg_color = Some(p.raised);
    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.lantern;
    v.error_fg_color = p.error;
    v.warn_fg_color = p.lantern;
    v.selection = egui::style::Selection {
        bg_fill: p.lantern.gamma_multiply(0.35),
        stroke: Stroke::new(1.5, p.lantern),
    };
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.window_stroke = Stroke::new(1.0, p.rule);
    let fills = [
        (&mut v.widgets.noninteractive, p.base),
        (&mut v.widgets.inactive, p.raised),
        (&mut v.widgets.hovered, p.band),
        (&mut v.widgets.active, p.band),
        (&mut v.widgets.open, p.band),
    ];
    for (w, fill) in fills {
        w.corner_radius = CornerRadius::same(4);
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.fg_stroke = Stroke::new(1.0, p.text);
        w.bg_stroke = Stroke::NONE;
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.rule);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.muted);
    // Checkbox/radio interiors use `bg_fill`, buttons `weak_bg_fill`: the
    // boxes get the band so they read on raised window backgrounds.
    v.widgets.inactive.bg_fill = p.band;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.edge);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.muted);
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.lantern);
    v
}

/// Install both variants, the type scale, and select the active one.
pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_visuals_of(egui::Theme::Dark, visuals(DARK, true));
    ctx.set_visuals_of(egui::Theme::Light, visuals(LIGHT, false));
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            let sizes = [
                (TextStyle::Body, BODY_SIZE),
                (TextStyle::Button, BODY_SIZE),
                (TextStyle::Small, SMALL_SIZE),
                (TextStyle::Heading, 18.0),
            ];
            for (text_style, size) in sizes {
                style
                    .text_styles
                    .insert(text_style, egui::FontId::proportional(size));
            }
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 4.0);
            style.spacing.menu_margin = egui::Margin::same(6);
            style.spacing.interact_size.y = 26.0;
        });
    }
    ctx.set_theme(match mode {
        ThemeMode::Dark => egui::ThemePreference::Dark,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::System => egui::ThemePreference::System,
    });
}

/// Paint the 14 px row icon for `kind` centred in `rect`: a filled folder,
/// or an outlined page with a folded corner. Shapes, not emoji glyphs (the
/// fallback emoji font rendered them at mismatched sizes and weights).
pub fn paint_icon(painter: &egui::Painter, rect: egui::Rect, kind: IconKind, color: Color32) {
    let c = rect.center();
    if kind == IconKind::Folder {
        let body = egui::Rect::from_center_size(c + egui::vec2(0.0, 1.0), egui::vec2(14.0, 10.0));
        let tab = egui::Rect::from_min_size(body.min - egui::vec2(0.0, 2.5), egui::vec2(6.0, 3.0));
        painter.rect_filled(tab, CornerRadius::same(1), color);
        painter.rect_filled(body, CornerRadius::same(2), color);
        return;
    }
    let w = 10.0;
    let h = 13.0;
    let fold = 3.5;
    let min = c - egui::vec2(w / 2.0, h / 2.0);
    let pts = vec![
        min,
        min + egui::vec2(w - fold, 0.0),
        min + egui::vec2(w, fold),
        min + egui::vec2(w, h),
        min + egui::vec2(0.0, h),
    ];
    painter.add(egui::Shape::closed_line(pts, Stroke::new(1.2, color)));
    painter.line_segment(
        [
            min + egui::vec2(w - fold, 0.0),
            min + egui::vec2(w - fold, fold),
        ],
        Stroke::new(1.2, color),
    );
    painter.line_segment(
        [min + egui::vec2(w - fold, fold), min + egui::vec2(w, fold)],
        Stroke::new(1.2, color),
    );
}

/// Name text with matched substrings lit in the accent.
pub fn highlight_job(name: &str, ranges: &[(usize, usize)], p: Palette, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let base_fmt = TextFormat {
        font_id: egui::FontId::proportional(size),
        color: p.text,
        ..Default::default()
    };
    let hit_fmt = TextFormat {
        font_id: egui::FontId::proportional(size),
        color: p.match_fg,
        background: p.match_bg,
        ..Default::default()
    };
    let mut pos = 0;
    for &(s, e) in ranges {
        if s > name.len() || e > name.len() || s >= e || s < pos {
            continue;
        }
        if !name.is_char_boundary(s) || !name.is_char_boundary(e) {
            continue;
        }
        if s > pos {
            job.append(&name[pos..s], 0.0, base_fmt.clone());
        }
        job.append(&name[s..e], 0.0, hit_fmt.clone());
        pos = e;
    }
    if pos < name.len() {
        job.append(&name[pos..], 0.0, base_fmt);
    } else if pos == 0 {
        job.append(name, 0.0, base_fmt);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_mode_round_trips() {
        assert_eq!(ThemeMode::from_str("dark"), ThemeMode::Dark);
        assert_eq!(ThemeMode::from_str("light"), ThemeMode::Light);
        assert_eq!(ThemeMode::from_str("system"), ThemeMode::System);
        assert_eq!(ThemeMode::from_str("bogus"), ThemeMode::System);
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::System] {
            assert_eq!(ThemeMode::from_str(mode.as_str()), mode);
        }
        assert_eq!(ThemeMode::default(), ThemeMode::System);
    }

    #[test]
    fn visuals_use_the_palette_and_one_accent() {
        for (p, dark) in [(DARK, true), (LIGHT, false)] {
            let v = visuals(p, dark);
            assert_eq!(v.dark_mode, dark);
            assert_eq!(v.panel_fill, p.base);
            assert_eq!(v.hyperlink_color, p.lantern);
            assert_eq!(v.selection.stroke.color, p.lantern);
            assert_eq!(v.window_shadow, egui::Shadow::NONE);
            assert_eq!(v.override_text_color, Some(p.text));
        }
    }

    /// Text and muted text stay readable on the background (WCAG AA 4.5:1
    /// for body text) in both themes.
    #[test]
    fn text_contrast_meets_aa() {
        fn lum(c: Color32) -> f64 {
            let ch = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
        }
        let ratio = |a: Color32, b: Color32| {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        for p in [DARK, LIGHT] {
            assert!(ratio(p.text, p.base) >= 4.5);
            assert!(
                ratio(p.muted, p.base) >= 4.5,
                "{:?}",
                ratio(p.muted, p.base)
            );
            assert!(ratio(p.text, p.band) >= 4.5);
        }
    }

    #[test]
    fn highlight_job_covers_the_whole_name() {
        let job = highlight_job("FlokiDesign", &[(0, 3)], DARK, 14.0);
        assert_eq!(job.text, "FlokiDesign");
        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[0].format.color, DARK.lantern);
        let plain = highlight_job("abc", &[], DARK, 14.0);
        assert_eq!(plain.text, "abc");
        assert_eq!(plain.sections.len(), 1);
    }
}
