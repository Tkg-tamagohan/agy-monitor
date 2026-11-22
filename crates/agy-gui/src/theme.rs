use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg_card: egui::Color32,
    pub bg_subtle: egui::Color32,
    pub border: egui::Color32,
    pub text_primary: egui::Color32,
    pub text_muted: egui::Color32,
    pub ok: egui::Color32,
    pub warn: egui::Color32,
    pub danger: egui::Color32,
    pub accent: egui::Color32,
    pub cache_line: egui::Color32,
    pub bucket_user: egui::Color32,
    pub bucket_reply: egui::Color32,
    pub bucket_thinking: egui::Color32,
    pub bucket_tool_args: egui::Color32,
    pub bucket_tool_out: egui::Color32,
    pub bucket_system: egui::Color32,
    pub bucket_checkpoint: egui::Color32,
    pub bucket_free: egui::Color32,
}

impl Palette {
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self {
                bg_card: egui::Color32::from_rgb(30, 34, 42),
                bg_subtle: egui::Color32::from_rgb(24, 27, 34),
                border: egui::Color32::from_rgb(54, 60, 74),
                text_primary: egui::Color32::from_rgb(232, 236, 244),
                text_muted: egui::Color32::from_rgb(152, 160, 176),
                ok: egui::Color32::from_rgb(66, 184, 131),
                warn: egui::Color32::from_rgb(240, 173, 78),
                danger: egui::Color32::from_rgb(235, 87, 87),
                accent: egui::Color32::from_rgb(91, 155, 245),
                cache_line: egui::Color32::from_rgb(76, 201, 240),
                bucket_user: egui::Color32::from_rgb(91, 155, 245),
                bucket_reply: egui::Color32::from_rgb(66, 184, 131),
                bucket_thinking: egui::Color32::from_rgb(168, 128, 245),
                bucket_tool_args: egui::Color32::from_rgb(240, 173, 78),
                bucket_tool_out: egui::Color32::from_rgb(235, 118, 87),
                bucket_system: egui::Color32::from_rgb(122, 132, 148),
                bucket_checkpoint: egui::Color32::from_rgb(76, 201, 240),
                bucket_free: egui::Color32::from_rgb(45, 50, 62),
            },
            ThemeMode::Light => Self {
                bg_card: egui::Color32::from_rgb(247, 248, 250),
                bg_subtle: egui::Color32::from_rgb(236, 239, 244),
                border: egui::Color32::from_rgb(208, 213, 221),
                text_primary: egui::Color32::from_rgb(26, 32, 44),
                text_muted: egui::Color32::from_rgb(100, 110, 128),
                ok: egui::Color32::from_rgb(22, 148, 92),
                warn: egui::Color32::from_rgb(212, 132, 18),
                danger: egui::Color32::from_rgb(205, 48, 48),
                accent: egui::Color32::from_rgb(38, 110, 224),
                cache_line: egui::Color32::from_rgb(14, 146, 194),
                bucket_user: egui::Color32::from_rgb(56, 126, 235),
                bucket_reply: egui::Color32::from_rgb(36, 160, 105),
                bucket_thinking: egui::Color32::from_rgb(140, 96, 222),
                bucket_tool_args: egui::Color32::from_rgb(224, 148, 34),
                bucket_tool_out: egui::Color32::from_rgb(218, 92, 62),
                bucket_system: egui::Color32::from_rgb(130, 140, 156),
                bucket_checkpoint: egui::Color32::from_rgb(24, 162, 208),
                bucket_free: egui::Color32::from_rgb(222, 226, 234),
            },
        }
    }

    pub fn usage_color(&self, used_percent: f64) -> egui::Color32 {
        if used_percent >= 85.0 {
            self.danger
        } else if used_percent >= 70.0 {
            self.warn
        } else {
            self.ok
        }
    }
}

pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode) {
    match mode {
        ThemeMode::Dark => ctx.set_visuals(egui::Visuals::dark()),
        ThemeMode::Light => ctx.set_visuals(egui::Visuals::light()),
    }
}
