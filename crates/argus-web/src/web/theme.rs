//! The Argus design tokens (docs/design/README.md) applied to egui.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0C, 0x0C, 0x0E);
pub const SURFACE: Color32 = Color32::from_rgb(0x15, 0x15, 0x18);
pub const LINE: Color32 = Color32::from_rgb(0x23, 0x23, 0x27);
pub const TEXT: Color32 = Color32::from_rgb(0xED, 0xED, 0xEF);
pub const TEXT_2: Color32 = Color32::from_rgb(0x8E, 0x8E, 0x96);
pub const TEXT_3: Color32 = Color32::from_rgb(0x5C, 0x5C, 0x64);
pub const ACCENT: Color32 = Color32::from_rgb(0x8A, 0xB4, 0xF8);
pub const ALERT: Color32 = Color32::from_rgb(0xEF, 0x6F, 0x6C);

pub const TILE_RADIUS: u8 = 10;
pub const PILL_RADIUS: u8 = 22;

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();
    for (name, bytes) in [
        (
            "geist",
            &include_bytes!("../../assets/fonts/Geist-Regular.ttf")[..],
        ),
        (
            "geist-semibold",
            &include_bytes!("../../assets/fonts/Geist-SemiBold.ttf")[..],
        ),
        (
            "geist-mono",
            &include_bytes!("../../assets/fonts/GeistMono-Regular.ttf")[..],
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    fonts
        .families
        .insert(FontFamily::Proportional, vec!["geist".into()]);
    fonts
        .families
        .insert(FontFamily::Monospace, vec!["geist-mono".into()]);
    fonts.families.insert(
        FontFamily::Name("semibold".into()),
        vec!["geist-semibold".into()],
    );
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = BG;
    v.faint_bg_color = SURFACE;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.linear_multiply(0.3);
    v.selection.stroke = Stroke::new(1.5, ACCENT);
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.bg_fill = SURFACE;
        w.weak_bg_fill = SURFACE;
        w.bg_stroke = Stroke::new(1.0, LINE);
        w.corner_radius = egui::CornerRadius::same(PILL_RADIUS);
        w.fg_stroke = Stroke::new(1.0, TEXT_2);
    }
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    ctx.set_visuals(v);
}
