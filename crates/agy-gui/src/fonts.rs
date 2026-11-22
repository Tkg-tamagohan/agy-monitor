use std::path::PathBuf;

use eframe::egui;

const CANDIDATE_FONTS: &[&str] = &["YuGothR.ttc", "YuGothM.ttc", "meiryo.ttc", "msgothic.ttc"];

pub fn install_japanese_fonts(ctx: &egui::Context) -> Option<PathBuf> {
    let mut fonts = egui::FontDefinitions::default();
    let loaded = load_first_available_font();
    if let Some((path, bytes)) = loaded {
        fonts.font_data.insert(
            "jp_system".to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "jp_system".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("jp_system".to_owned());
        ctx.set_fonts(fonts);
        return Some(path);
    }
    ctx.set_fonts(fonts);
    None
}

fn load_first_available_font() -> Option<(PathBuf, Vec<u8>)> {
    let windir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let fonts_dir = windir.join("Fonts");

    for name in CANDIDATE_FONTS {
        let candidate = fonts_dir.join(name);
        if candidate.is_file()
            && let Ok(bytes) = std::fs::read(&candidate)
        {
            return Some((candidate, bytes));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_01_japanese_font_discovery_and_registration() {
        let ctx = egui::Context::default();
        let installed = install_japanese_fonts(&ctx);
        if cfg!(windows) {
            assert!(
                installed.is_some(),
                "expected at least one Japanese system font in Windows\\Fonts"
            );
        }
    }
}
