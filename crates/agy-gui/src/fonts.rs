use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

struct FontCandidate {
    file_name: &'static str,
    proportional_index: u32,
    monospace_index: u32,
    tweak: fn() -> egui::FontTweak,
}

fn default_tweak() -> egui::FontTweak {
    egui::FontTweak::default()
}

fn noto_sans_jp_tweak() -> egui::FontTweak {
    egui::FontTweak {
        coords: egui::epaint::text::VariationCoords::new([("wght", 400.0)]),
        ..Default::default()
    }
}

// Candidates are strictly ordered to avoid faces with huge line_gap (such as YuGothR.ttc index 0).
const CANDIDATE_FONTS: &[FontCandidate] = &[
    FontCandidate {
        // [0] BIZ UDGothic (fixed-width), [1] BIZ UDPGothic (proportional), line_gap = 0
        file_name: "BIZ-UDGothicR.ttc",
        proportional_index: 1,
        monospace_index: 0,
        tweak: default_tweak,
    },
    FontCandidate {
        // [2] Meiryo UI (compact descent), line_gap = 0
        file_name: "meiryo.ttc",
        proportional_index: 2,
        monospace_index: 2,
        tweak: default_tweak,
    },
    FontCandidate {
        // Variable font defaults to wght=100 unless 400 is pinned via VariationCoords
        file_name: "NotoSansJP-VF.ttf",
        proportional_index: 0,
        monospace_index: 0,
        tweak: noto_sans_jp_tweak,
    },
    FontCandidate {
        // [1] Yu Gothic UI Regular, line_gap = 0
        file_name: "YuGothM.ttc",
        proportional_index: 1,
        monospace_index: 1,
        tweak: default_tweak,
    },
    FontCandidate {
        // [1] MS UI Gothic, [0] MS Gothic, line_gap = 0
        file_name: "msgothic.ttc",
        proportional_index: 1,
        monospace_index: 0,
        tweak: default_tweak,
    },
];

/// Registers Windows Japanese fonts as fallbacks after egui's default Latin fonts.
/// Keeping Latin fonts first preserves balanced row metrics and avoids `\` -> `¥` substitution.
pub fn install_japanese_fonts(ctx: &egui::Context) -> Option<PathBuf> {
    if let Some((path, bytes, candidate)) = load_first_available_font() {
        install_candidate_font(ctx, candidate, bytes);
        return Some(path);
    }
    ctx.set_fonts(egui::FontDefinitions::default());
    None
}

fn install_candidate_font(ctx: &egui::Context, candidate: &FontCandidate, bytes: Vec<u8>) {
    let mut fonts = egui::FontDefinitions::default();
    let blob: egui::epaint::text::Blob = Arc::new(bytes);
    let tweak = (candidate.tweak)();

    let prop_data = egui::FontData::from_blob(Arc::clone(&blob), candidate.proportional_index)
        .tweak(tweak.clone());
    fonts
        .font_data
        .insert("jp_proportional".to_owned(), prop_data.into());
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("jp_proportional".to_owned());

    let mono_data = egui::FontData::from_blob(blob, candidate.monospace_index).tweak(tweak);
    fonts
        .font_data
        .insert("jp_monospace".to_owned(), mono_data.into());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("jp_monospace".to_owned());

    ctx.set_fonts(fonts);
}

fn windows_fonts_dir() -> PathBuf {
    let windir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    windir.join("Fonts")
}

fn load_first_available_font() -> Option<(PathBuf, Vec<u8>, &'static FontCandidate)> {
    let fonts_dir = windows_fonts_dir();

    for candidate in CANDIDATE_FONTS {
        let path = fonts_dir.join(candidate.file_name);
        if path.is_file()
            && let Ok(bytes) = std::fs::read(&path)
        {
            return Some((path, bytes, candidate));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TextRenderMetrics {
        bounds: egui::Rect,
        vertex_count: usize,
    }

    fn collect_text_metrics(
        shapes: &[egui::epaint::ClippedShape],
        target_text: &str,
    ) -> Option<TextRenderMetrics> {
        fn visit(shape: &egui::Shape, target_text: &str) -> Option<TextRenderMetrics> {
            match shape {
                egui::Shape::Text(ts) if ts.galley.text() == target_text => {
                    let mut bounds = egui::Rect::NOTHING;
                    let mut vertex_count = 0usize;
                    for row in &ts.galley.rows {
                        if !row.visuals.mesh.is_empty() {
                            vertex_count += row.visuals.mesh.vertices.len();
                            let r = row
                                .visuals
                                .mesh_bounds
                                .translate(ts.pos.to_vec2() + row.pos.to_vec2());
                            bounds = bounds.union(r);
                        }
                    }
                    if !bounds.is_positive() {
                        bounds = ts.visual_bounding_rect();
                    }
                    Some(TextRenderMetrics {
                        bounds,
                        vertex_count,
                    })
                }
                egui::Shape::Vec(vec) => vec.iter().find_map(|s| visit(s, target_text)),
                _ => None,
            }
        }
        shapes.iter().find_map(|cs| visit(&cs.shape, target_text))
    }

    fn assert_japanese_fallback_registered(ctx: &egui::Context, font_label: &str) {
        let mut init_out = ctx.run_ui(egui::RawInput::default(), |_| {});
        init_out.textures_delta.clear();

        let defs = ctx.fonts(|f| f.definitions().clone());
        let prop_list = defs
            .families
            .get(&egui::FontFamily::Proportional)
            .expect("Proportional family missing");
        let mono_list = defs
            .families
            .get(&egui::FontFamily::Monospace)
            .expect("Monospace family missing");
        assert_ne!(
            prop_list.first().map(String::as_str),
            Some("jp_proportional"),
            "[{font_label}] Japanese proportional font must be a fallback after default Latin font"
        );
        assert_eq!(
            prop_list.last().map(String::as_str),
            Some("jp_proportional"),
            "[{font_label}] jp_proportional must be at Proportional fallback tail"
        );
        assert_ne!(
            mono_list.first().map(String::as_str),
            Some("jp_monospace"),
            "[{font_label}] Japanese monospace font must be a fallback after default Latin font"
        );
        assert_eq!(
            mono_list.last().map(String::as_str),
            Some("jp_monospace"),
            "[{font_label}] jp_monospace must be at Monospace fallback tail"
        );
    }

    fn verify_selectable_label_metrics(ctx: &egui::Context, font_label: &str) -> TextRenderMetrics {
        let mut btn_rects: Vec<(&'static str, egui::Rect)> = Vec::new();
        let mut row_h_14 = 0.0_f32;

        let labels = ["リアルタイム監視", "ステップ明細 (94)", "Light"];

        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            row_h_14 = ui.fonts_mut(|f| f.row_height(&egui::FontId::proportional(14.0)));
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    for label in labels {
                        let resp = ui.selectable_label(true, label);
                        btn_rects.push((label, resp.rect));
                    }
                });
            });
        });
        let shapes = std::mem::take(&mut output.shapes);
        output.textures_delta.clear();

        assert!(
            row_h_14 <= 19.0,
            "[{font_label}] row_height at 14pt should be compact (<= 19pt), got {row_h_14}"
        );

        let mut realtime_metrics = None;
        for (label, btn_rect) in btn_rects {
            let metrics = collect_text_metrics(&shapes, label)
                .unwrap_or_else(|| panic!("[{font_label}] missing TextShape for {label:?}"));
            let delta = (metrics.bounds.center().y - btn_rect.center().y).abs();
            eprintln!(
                "[{font_label}] label={label:?} row_h={row_h_14:.2} btn_y=[{:.2}..{:.2}] (mid={:.2}) text_y=[{:.2}..{:.2}] (mid={:.2}) delta={delta:.2} verts={}",
                btn_rect.top(),
                btn_rect.bottom(),
                btn_rect.center().y,
                metrics.bounds.top(),
                metrics.bounds.bottom(),
                metrics.bounds.center().y,
                metrics.vertex_count
            );
            assert!(
                delta <= 1.5,
                "[{font_label}] vertical center mismatch for {label:?}: delta={delta:.2}pt (btn_mid={:.2}, text_mid={:.2})",
                btn_rect.center().y,
                metrics.bounds.center().y
            );
            if label == "リアルタイム監視" {
                realtime_metrics = Some(metrics);
            }
        }
        realtime_metrics.expect("リアルタイム監視 metrics must be collected")
    }

    #[test]
    fn gui_01_japanese_font_discovery_and_registration() {
        let ctx = egui::Context::default();
        let installed = install_japanese_fonts(&ctx);
        if cfg!(windows) {
            let path = installed.expect("expected a Japanese system font in Windows\\Fonts");
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            assert_ne!(
                file_name, "YuGothR.ttc",
                "YuGothR.ttc has oversized lineGap and should not be chosen"
            );
            assert_japanese_fallback_registered(&ctx, file_name);
        }
    }

    #[test]
    fn gui_05_button_text_vertically_centered_and_compact_row_height() {
        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        verify_selectable_label_metrics(&ctx, "selected");
    }

    #[cfg(windows)]
    #[test]
    fn gui_10_all_existing_candidate_fonts_render_properly() {
        let fonts_dir = windows_fonts_dir();
        let existing: Vec<(&FontCandidate, PathBuf)> = CANDIDATE_FONTS
            .iter()
            .filter_map(|candidate| {
                let path = fonts_dir.join(candidate.file_name);
                path.is_file().then_some((candidate, path))
            })
            .collect();

        assert!(
            !existing.is_empty(),
            "expected at least one candidate font in {}",
            fonts_dir.display()
        );

        for (candidate, path) in existing {
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            let ctx = egui::Context::default();
            install_candidate_font(&ctx, candidate, bytes);

            assert_japanese_fallback_registered(&ctx, candidate.file_name);
            let metrics = verify_selectable_label_metrics(&ctx, candidate.file_name);

            assert!(
                metrics.vertex_count >= 4 * 8,
                "[{}] expected non-empty glyph mesh for 8 Japanese chars (verts >= {}), got {}",
                candidate.file_name,
                4 * 8,
                metrics.vertex_count
            );
            assert!(
                metrics.bounds.width() > 20.0 && metrics.bounds.height() >= 8.0,
                "[{}] unexpected glyph bounds for 'リアルタイム監視': {:?}",
                candidate.file_name,
                metrics.bounds
            );
        }
    }
}
