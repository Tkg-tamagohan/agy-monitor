use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agy_core::analyzer::{ConversationAnalysis, StepSummary};
use agy_core::models::format_model_short_label;
use agy_core::monitor::{MonitorCommand, MonitorConfig, MonitorHandle, MonitorSnapshot};
use agy_core::paths::GeminiPaths;
use agy_core::sessions::SessionSummary;
use chrono::{DateTime, Local, TimeZone, Utc};
use eframe::egui;
use egui_plot::{Corner, HLine, Legend, Line, Plot, PlotPoints, Points, VLine};

use crate::fonts::install_japanese_fonts;
use crate::theme::{Palette, ThemeMode, apply_theme};
use crate::usage_view::{UsageViewState, render_usage_phase2};

const COMPACT_SIZE: egui::Vec2 = egui::vec2(380.0, 170.0);
const DEFAULT_NORMAL_SIZE: egui::Vec2 = egui::vec2(1240.0, 820.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainTab {
    Realtime,
    UsagePhase2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Steps,
    HeavyTools,
}

pub struct AgyMonitorApp {
    monitor: MonitorHandle,
    main_tab: MainTab,
    detail_tab: DetailTab,
    steps_newest_first: bool,
    theme_mode: ThemeMode,
    always_on_top: bool,
    compact_mode: bool,
    saved_normal_size: egui::Vec2,
    project_filter: String,
    search_query: String,
    expanded_roots: HashSet<String>,
    copied_until: Option<Instant>,
    usage_view_state: UsageViewState,
    phase2_initialized: bool,
}

impl AgyMonitorApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: GeminiPaths) -> Self {
        install_japanese_fonts(&cc.egui_ctx);
        let theme_mode = ThemeMode::Dark;
        apply_theme(&cc.egui_ctx, theme_mode);

        let egui_ctx = cc.egui_ctx.clone();
        let mut config = MonitorConfig::new(paths);
        config.notify = Some(Arc::new(move || {
            egui_ctx.request_repaint();
        }));
        let monitor = MonitorHandle::start(config);

        Self {
            monitor,
            main_tab: MainTab::Realtime,
            detail_tab: DetailTab::Steps,
            steps_newest_first: true,
            theme_mode,
            always_on_top: false,
            compact_mode: false,
            saved_normal_size: DEFAULT_NORMAL_SIZE,
            project_filter: String::new(),
            search_query: String::new(),
            expanded_roots: HashSet::new(),
            copied_until: None,
            usage_view_state: UsageViewState::default(),
            phase2_initialized: false,
        }
    }

    fn toggle_always_on_top(&mut self, ctx: &egui::Context) {
        self.always_on_top = !self.always_on_top;
        let level = if self.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    }

    fn toggle_compact_mode(&mut self, ctx: &egui::Context) {
        if !self.compact_mode {
            if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                let sz = rect.size();
                if sz.x >= 420.0 && sz.y >= 240.0 {
                    self.saved_normal_size = sz;
                }
            }
            self.compact_mode = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(COMPACT_SIZE));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(COMPACT_SIZE));
        } else {
            self.compact_mode = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                760.0, 520.0,
            )));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(self.saved_normal_size));
        }
    }

    fn toggle_theme(&mut self, ctx: &egui::Context) {
        self.theme_mode = match self.theme_mode {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        };
        apply_theme(ctx, self.theme_mode);
    }
}

impl eframe::App for AgyMonitorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.monitor.snapshot();
        self.render_ui(ui, &snap);
    }
}

impl AgyMonitorApp {
    fn render_ui(&mut self, ui: &mut egui::Ui, snap: &MonitorSnapshot) {
        let ctx = ui.ctx().clone();
        let palette = Palette::for_mode(self.theme_mode);

        if self.compact_mode {
            self.render_compact_hud(ui, &ctx, snap, &palette);
            return;
        }

        self.render_top_bar(ui, &ctx, snap, &palette);

        match self.main_tab {
            MainTab::UsagePhase2 => {
                if !self.phase2_initialized {
                    self.phase2_initialized = true;
                    self.monitor.send(MonitorCommand::RefreshUsage(
                        self.usage_view_state.selected_period,
                    ));
                    self.monitor.send(MonitorCommand::RefreshCustomizations);
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    let actions = render_usage_phase2(
                        ui,
                        &mut self.usage_view_state,
                        snap.usage_report.as_ref(),
                        snap.usage_loading,
                        snap.customizations.as_ref(),
                        snap.customizations_loading,
                        &palette,
                    );
                    if let Some(period) = actions.refresh_usage {
                        self.monitor.send(MonitorCommand::RefreshUsage(period));
                    }
                    if actions.refresh_customizations {
                        self.monitor.send(MonitorCommand::RefreshCustomizations);
                    }
                });
            }
            MainTab::Realtime => {
                self.render_sidebar(ui, snap, &palette);
                self.render_main_pane(ui, &ctx, snap, &palette);
            }
        }
    }

    fn render_top_bar(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        snap: &MonitorSnapshot,
        palette: &Palette,
    ) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.main_tab, MainTab::Realtime, "リアルタイム監視");
                ui.selectable_value(&mut self.main_tab, MainTab::UsagePhase2, "期間統計・棚卸し");

                ui.separator();

                let mut follow = snap.follow_latest;
                if ui.toggle_value(&mut follow, "最新追従").changed() {
                    self.monitor.send(MonitorCommand::SetFollowLatest(follow));
                }

                if ui.selectable_label(self.always_on_top, "最前面").clicked() {
                    self.toggle_always_on_top(ctx);
                }

                if ui.button("コンパクト").clicked() {
                    self.toggle_compact_mode(ctx);
                }

                if ui.button("更新").clicked() {
                    self.monitor.send(MonitorCommand::RefreshNow);
                }

                let pause_label = if snap.paused {
                    "再開"
                } else {
                    "一時停止"
                };
                if ui.selectable_label(snap.paused, pause_label).clicked() {
                    self.monitor.send(MonitorCommand::SetPaused(!snap.paused));
                }

                let theme_label = match self.theme_mode {
                    ThemeMode::Dark => "Light",
                    ThemeMode::Light => "Dark",
                };
                if ui.button(theme_label).clicked() {
                    self.toggle_theme(ctx);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ts) = snap.last_updated_at {
                        let local_ts: DateTime<Local> = ts.with_timezone(&Local);
                        ui.label(
                            egui::RichText::new(local_ts.format("%H:%M:%S").to_string())
                                .monospace()
                                .color(palette.text_muted),
                        );
                    }
                    if snap.db_busy {
                        ui.label(
                            egui::RichText::new("DBロック中")
                                .color(palette.warn)
                                .strong(),
                        );
                    }
                    if let Some(err) = &snap.last_error {
                        ui.label(egui::RichText::new(err).color(palette.danger).small());
                    }
                });
            });
            ui.add_space(2.0);
        });
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui, snap: &MonitorSnapshot, palette: &Palette) {
        egui::Panel::left("session_sidebar")
            .resizable(true)
            .default_size(290.0)
            .min_size(220.0)
            .max_size(460.0)
            .show(ui, |ui| {
                ui.add_space(4.0);

                // Clear stale project filter if that project disappeared on reload.
                if !self.project_filter.is_empty()
                    && !snap
                        .index
                        .projects
                        .iter()
                        .any(|p| p.key == self.project_filter)
                {
                    self.project_filter.clear();
                }

                // Project filter
                let selected_proj_label = if self.project_filter.is_empty() {
                    "すべて".to_string()
                } else {
                    snap.index
                        .projects
                        .iter()
                        .find(|p| p.key == self.project_filter)
                        .map(|p| p.label.clone())
                        .unwrap_or_else(|| "すべて".to_string())
                };
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("project_filter")
                        .width(ui.available_width())
                        .selected_text(selected_proj_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.project_filter, String::new(), "すべて");
                            for proj in &snap.index.projects {
                                ui.selectable_value(
                                    &mut self.project_filter,
                                    proj.key.clone(),
                                    &proj.label,
                                );
                            }
                        });
                });

                ui.add_space(4.0);

                // Search input
                ui.horizontal(|ui| {
                    let query_empty = self.search_query.is_empty();
                    let desired_w = if query_empty {
                        ui.available_width()
                    } else {
                        (ui.available_width() - 26.0).max(80.0)
                    };
                    let edit = egui::TextEdit::singleline(&mut self.search_query)
                        .hint_text("タイトル・ID検索...")
                        .desired_width(desired_w);
                    ui.add(edit);
                    if !query_empty && ui.button("×").clicked() {
                        self.search_query.clear();
                    }
                });

                ui.separator();

                let q = self.search_query.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut any_shown = false;
                        for node in &snap.index.roots {
                            let root = &node.summary;
                            if !self.project_filter.is_empty()
                                && root.project_key != self.project_filter
                            {
                                continue;
                            }

                            let root_matches = q.is_empty() || session_matches_query(root, &q);
                            let matching_children: Vec<&SessionSummary> = node
                                .children
                                .iter()
                                .filter(|c| q.is_empty() || session_matches_query(c, &q))
                                .collect();

                            if !root_matches && matching_children.is_empty() {
                                continue;
                            }
                            any_shown = true;

                            let has_children = !node.children.is_empty();
                            let is_expanded = if !q.is_empty() && !matching_children.is_empty() {
                                true
                            } else {
                                self.expanded_roots.contains(&root.conversation_id)
                            };
                            let is_selected =
                                snap.selected_id.as_deref() == Some(&root.conversation_id);

                            ui.horizontal(|ui| {
                                if has_children {
                                    let arrow = if is_expanded { "▼" } else { "▶" };
                                    if ui.small_button(arrow).clicked() {
                                        if is_expanded {
                                            self.expanded_roots.remove(&root.conversation_id);
                                        } else {
                                            self.expanded_roots
                                                .insert(root.conversation_id.clone());
                                        }
                                    }
                                } else {
                                    ui.add_space(18.0);
                                }

                                let time_str = format_mtime_short(root.effective_mtime_ms);
                                let sub_tag = if has_children {
                                    format!(" [{}]", node.children.len())
                                } else {
                                    String::new()
                                };
                                let short_model = format_model_short_label(&root.model_label);
                                let proj_or_surface = if root.project_label.is_empty() {
                                    root.surface.label()
                                } else {
                                    &root.project_label
                                };

                                let mut job = egui::text::LayoutJob::default();
                                job.append(
                                    &truncate_ui(
                                        &root.display_title,
                                        if short_model.is_empty() { 34 } else { 26 },
                                    ),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::proportional(13.0),
                                        color: palette.text_primary,
                                        ..Default::default()
                                    },
                                );
                                if !short_model.is_empty() {
                                    job.append(
                                        &format!("  {short_model}"),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::proportional(11.5),
                                            color: palette.accent,
                                            ..Default::default()
                                        },
                                    );
                                }
                                if !sub_tag.is_empty() {
                                    job.append(
                                        &sub_tag,
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::proportional(11.5),
                                            color: palette.text_muted,
                                            ..Default::default()
                                        },
                                    );
                                }
                                job.append(
                                    &format!("\n{proj_or_surface} · {time_str}"),
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::proportional(11.5),
                                        color: palette.text_muted,
                                        ..Default::default()
                                    },
                                );

                                let mut resp = ui.selectable_label(is_selected, job);
                                if !root.model_label.is_empty() {
                                    resp = resp.on_hover_text(format!(
                                        "{}\nモデル: {}",
                                        root.display_title, root.model_label
                                    ));
                                } else {
                                    resp = resp.on_hover_text(&root.display_title);
                                }
                                if resp.clicked() {
                                    self.monitor.send(MonitorCommand::SetFollowLatest(false));
                                    self.monitor.send(MonitorCommand::SelectConversation(Some(
                                        root.conversation_id.clone(),
                                    )));
                                }
                            });

                            if is_expanded {
                                let children_to_show: Vec<&SessionSummary> = if q.is_empty() {
                                    node.children.iter().collect()
                                } else {
                                    matching_children
                                };
                                for child in children_to_show {
                                    let child_selected =
                                        snap.selected_id.as_deref() == Some(&child.conversation_id);
                                    ui.horizontal(|ui| {
                                        ui.add_space(26.0);
                                        let short_model =
                                            format_model_short_label(&child.model_label);
                                        let mut job = egui::text::LayoutJob::default();
                                        job.append(
                                            &format!(
                                                "↳ {}",
                                                truncate_ui(
                                                    &child.display_title,
                                                    if short_model.is_empty() { 26 } else { 20 }
                                                )
                                            ),
                                            0.0,
                                            egui::TextFormat {
                                                font_id: egui::FontId::proportional(12.5),
                                                color: palette.text_primary,
                                                ..Default::default()
                                            },
                                        );
                                        if !short_model.is_empty() {
                                            job.append(
                                                &format!("  {short_model}"),
                                                0.0,
                                                egui::TextFormat {
                                                    font_id: egui::FontId::proportional(11.0),
                                                    color: palette.accent,
                                                    ..Default::default()
                                                },
                                            );
                                        }
                                        job.append(
                                            &format!(
                                                " ({})",
                                                format_mtime_short(child.effective_mtime_ms)
                                            ),
                                            0.0,
                                            egui::TextFormat {
                                                font_id: egui::FontId::proportional(11.0),
                                                color: palette.text_muted,
                                                ..Default::default()
                                            },
                                        );

                                        let mut resp = ui.selectable_label(child_selected, job);
                                        if !child.model_label.is_empty() {
                                            resp = resp.on_hover_text(format!(
                                                "{}\nモデル: {}",
                                                child.display_title, child.model_label
                                            ));
                                        } else {
                                            resp = resp.on_hover_text(&child.display_title);
                                        }
                                        if resp.clicked() {
                                            self.monitor
                                                .send(MonitorCommand::SetFollowLatest(false));
                                            self.monitor.send(MonitorCommand::SelectConversation(
                                                Some(child.conversation_id.clone()),
                                            ));
                                        }
                                    });
                                }
                            }
                        }

                        if !any_shown {
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new("一致するセッションがありません")
                                    .color(palette.text_muted),
                            );
                        }
                    });
            });
    }

    fn render_main_pane(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        snap: &MonitorSnapshot,
        palette: &Palette,
    ) {
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(analysis) = snap.analysis.as_ref() else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("セッションが選択されていません")
                            .color(palette.text_muted),
                    );
                });
                return;
            };

            let summary = snap.index.by_id.get(&analysis.conversation_id);
            let title = summary
                .map(|s| s.display_title.as_str())
                .unwrap_or(&analysis.conversation_id);

            let avail_w = ui.available_width();
            let avail_h = ui.available_height();
            let side_by_side = avail_w >= 540.0;
            let plot_h = if avail_h >= 760.0 {
                185.0
            } else if avail_h >= 620.0 {
                150.0
            } else {
                125.0
            };

            let render_monitor_section = |ui: &mut egui::Ui, app: &mut Self| {
                app.render_session_header(ctx, ui, title, summary, analysis, palette);
                ui.add_space(8.0);
                app.render_kpi_cards(ui, analysis, palette);
                ui.add_space(8.0);
                app.render_buckets_bar(ui, analysis, palette);
                ui.add_space(10.0);
                app.render_plots(ui, analysis, palette, plot_h);
            };

            // Keep the monitor section fixed at the top while scrolling STEP details below.
            // On narrow panes or short windows, bound the upper monitor height so the bottom detail tabs stay visible.
            let reserved_detail_h = if avail_h < 320.0 {
                avail_h * 0.45
            } else {
                (avail_h * 0.30).clamp(150.0, 220.0)
            };
            let max_monitor_h = (avail_h - reserved_detail_h).max(160.0);
            if !side_by_side || avail_h < 680.0 {
                egui::ScrollArea::vertical()
                    .id_salt("monitor_upper_scroll")
                    .auto_shrink([false, true])
                    .max_height(max_monitor_h)
                    .show(ui, |ui| {
                        render_monitor_section(ui, self);
                    });
            } else {
                render_monitor_section(ui, self);
            }

            ui.add_space(8.0);
            ui.separator();
            self.render_bottom_details(ui, analysis, palette);
        });
    }

    fn render_session_header(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        title: &str,
        summary: Option<&SessionSummary>,
        analysis: &ConversationAnalysis,
        palette: &Palette,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(title);
            ui.add_space(4.0);

            egui::Frame::new()
                .fill(palette.bg_subtle)
                .stroke(egui::Stroke::new(1.0, palette.border))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(6, 2))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&analysis.model.model_label)
                                .color(palette.accent)
                                .strong(),
                        )
                        .extend(),
                    );
                });

            ui.add_space(4.0);

            let copied_active = self.copied_until.is_some_and(|t| t > Instant::now());
            let copy_lbl = if copied_active {
                "コピー済"
            } else {
                "IDコピー"
            };
            if ui.small_button(copy_lbl).clicked() {
                ctx.copy_text(analysis.conversation_id.clone());
                self.copied_until = Some(Instant::now() + Duration::from_secs(2));
                ctx.request_repaint_after(Duration::from_secs(2));
            }

            let badge_text = if analysis.exact { "実測" } else { "推定" };
            let badge_color = if analysis.exact {
                palette.ok
            } else {
                palette.warn
            };
            ui.label(
                egui::RichText::new(format!("[{badge_text}]"))
                    .color(badge_color)
                    .strong(),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "上限 {} ({})",
                        format_tokens(analysis.context_limit),
                        analysis.model.limit_source
                    ))
                    .color(palette.text_muted),
                );
                if let Some(s) = summary
                    && !s.project_label.is_empty()
                {
                    ui.label(
                        egui::RichText::new(format!("{} |", s.project_label)).color(palette.accent),
                    );
                }
            });
        });
    }

    fn render_kpi_cards(
        &self,
        ui: &mut egui::Ui,
        analysis: &ConversationAnalysis,
        palette: &Palette,
    ) {
        let avail = ui.available_width();
        let spacing = 8.0;
        let cols = if avail >= 580.0 { 4 } else { 2 };
        let card_w = ((avail - spacing * (cols as f32 - 1.0)) / cols as f32).max(120.0);

        let render_card_1 = |ui: &mut egui::Ui| {
            kpi_frame(ui, card_w, palette, |ui| {
                ui.label(
                    egui::RichText::new("コンテキスト使用率")
                        .small()
                        .color(palette.text_muted),
                );
                let col = palette.usage_color(analysis.used_percent);
                ui.label(
                    egui::RichText::new(format!("{:.1}%", analysis.used_percent))
                        .size(22.0)
                        .strong()
                        .color(col),
                );
                draw_threshold_bar(ui, analysis.used_percent, palette, 10.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_tokens(analysis.used_tokens),
                        format_tokens(analysis.context_limit)
                    ))
                    .small()
                    .color(palette.text_muted),
                );
            });
        };

        let render_card_2 = |ui: &mut egui::Ui| {
            kpi_frame(ui, card_w, palette, |ui| {
                ui.label(
                    egui::RichText::new("直近キャッシュ率")
                        .small()
                        .color(palette.text_muted),
                );
                ui.label(
                    egui::RichText::new(format!("{:.1}%", analysis.cache.cache_hit_ratio))
                        .size(22.0)
                        .strong()
                        .color(palette.cache_line),
                );
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_tokens(analysis.cache.cached_tokens),
                        format_tokens(analysis.cache.last_prompt_tokens)
                    ))
                    .small()
                    .color(palette.text_muted),
                );
            });
        };

        let render_card_3 = |ui: &mut egui::Ui| {
            kpi_frame(ui, card_w, palette, |ui| {
                ui.label(
                    egui::RichText::new("累計キャッシュ率")
                        .small()
                        .color(palette.text_muted),
                );
                ui.label(
                    egui::RichText::new(format!("{:.1}%", analysis.cache.session_cache_hit_ratio))
                        .size(22.0)
                        .strong()
                        .color(palette.accent),
                );
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_tokens(analysis.cache.cumulative_cached_tokens),
                        format_tokens(analysis.cache.cumulative_input_tokens)
                    ))
                    .small()
                    .color(palette.text_muted),
                );
            });
        };

        let render_card_4 = |ui: &mut egui::Ui| {
            kpi_frame(ui, card_w, palette, |ui| {
                ui.label(
                    egui::RichText::new("コンパクション")
                        .small()
                        .color(palette.text_muted),
                );
                ui.label(
                    egui::RichText::new(format!("{} 回", analysis.compactions.len()))
                        .size(22.0)
                        .strong()
                        .color(palette.text_primary),
                );
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(format!(
                        "削減 {}",
                        format_tokens(analysis.total_saved_tokens)
                    ))
                    .small()
                    .color(palette.text_muted),
                );
            });
        };

        if cols == 4 {
            ui.horizontal(|ui| {
                render_card_1(ui);
                render_card_2(ui);
                render_card_3(ui);
                render_card_4(ui);
            });
        } else {
            ui.horizontal(|ui| {
                render_card_1(ui);
                render_card_2(ui);
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                render_card_3(ui);
                render_card_4(ui);
            });
        }
    }

    fn render_buckets_bar(
        &self,
        ui: &mut egui::Ui,
        analysis: &ConversationAnalysis,
        palette: &Palette,
    ) {
        let b = &analysis.buckets;
        let custom_total = b
            .customization_breakdown
            .as_ref()
            .map(|c| {
                c.mcp_tools + c.mcp_instructions + c.rules_files + c.skills_index + c.custom_agents
            })
            .unwrap_or(0);
        let sys_total = b.system_tools + b.system_prompt + b.system_messages;
        let segments: [(&str, u64, egui::Color32); 9] = [
            ("ユーザー", b.user_messages, palette.bucket_user),
            ("応答", b.agent_replies, palette.bucket_reply),
            ("思考", b.agent_thinking, palette.bucket_thinking),
            ("ツール引数", b.tool_call_args, palette.bucket_tool_args),
            ("ツール出力", b.tool_call_outputs, palette.bucket_tool_out),
            ("カスタマイズ", custom_total, palette.accent),
            ("システム", sys_total, palette.bucket_system),
            (
                "圧縮バッファ",
                b.checkpoint_buffer,
                palette.bucket_checkpoint,
            ),
            ("空き", b.free_space, palette.bucket_free),
        ];

        let total = analysis.context_limit.max(analysis.used_tokens).max(1);

        egui::Frame::group(ui.style())
            .fill(palette.bg_card)
            .stroke(egui::Stroke::new(1.0, palette.border))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let bar_w = ui.available_width();
                let bar_h = 16.0;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(bar_w, bar_h), egui::Sense::hover());
                let painter = ui.painter_at(rect);
                let mut x = rect.left();
                for (idx, (_, val, color)) in segments.iter().enumerate() {
                    if *val == 0 {
                        continue;
                    }
                    let remain = (rect.right() - x).max(0.0);
                    let w = if idx == segments.len() - 1 {
                        remain
                    } else {
                        (((*val as f64 / total as f64) * bar_w as f64) as f32)
                            .max(1.0)
                            .min(remain)
                    };
                    let seg_rect =
                        egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, bar_h));
                    painter.rect_filled(seg_rect, 0.0, *color);
                    x += w;
                }

                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for (name, val, color) in segments {
                        if val == 0 && name != "空き" {
                            continue;
                        }
                        let (dot, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().rect_filled(dot, 2.0, color);
                        ui.label(
                            egui::RichText::new(format!("{name} {}", format_tokens(val)))
                                .small()
                                .color(palette.text_muted),
                        );
                        ui.add_space(6.0);
                    }
                });
            });
    }

    fn render_plots(
        &self,
        ui: &mut egui::Ui,
        analysis: &ConversationAnalysis,
        palette: &Palette,
        plot_h: f32,
    ) {
        let avail = ui.available_width();
        let side_by_side = avail >= 540.0;
        let plot_w = if side_by_side {
            ((avail - 10.0) / 2.0).max(240.0)
        } else {
            avail.max(200.0)
        };

        let render_ctx_plot = |ui: &mut egui::Ui| {
            ui.vertical(|ui| {
                ui.set_width(plot_w);
                ui.label(egui::RichText::new("コンテキスト推移").strong());
                let raw_pts: Vec<[f64; 2]> = analysis
                    .turns_history
                    .iter()
                    .enumerate()
                    .map(|(idx, t)| [idx as f64 + 1.0, t.prompt_tokens as f64])
                    .collect();

                Plot::new("ctx_history_plot")
                    .height(plot_h)
                    .width(plot_w)
                    .allow_scroll(false)
                    .allow_zoom(false)
                    .allow_drag(false)
                    .include_y(0.0)
                    .include_y(analysis.context_limit as f64 * 1.05)
                    .legend(Legend::default().position(Corner::LeftTop))
                    .show(ui, |plot_ui| {
                        plot_ui.line(
                            Line::new("コンテキスト量", PlotPoints::from(raw_pts.clone()))
                                .color(palette.accent)
                                .width(2.0),
                        );
                        if !raw_pts.is_empty() {
                            plot_ui.points(
                                Points::new("", PlotPoints::from(raw_pts))
                                    .color(palette.accent)
                                    .radius(3.0),
                            );
                        }
                        plot_ui.hline(
                            HLine::new("上限", analysis.context_limit as f64).color(palette.danger),
                        );
                        let mut legend_emitted = false;
                        for (idx, t) in analysis.turns_history.iter().enumerate() {
                            if t.compacted {
                                let name = if legend_emitted { "" } else { "圧縮" };
                                legend_emitted = true;
                                plot_ui
                                    .vline(VLine::new(name, idx as f64 + 1.0).color(palette.warn));
                            }
                        }
                    });
            });
        };

        let render_cache_plot = |ui: &mut egui::Ui| {
            ui.vertical(|ui| {
                ui.set_width(plot_w);
                ui.label(egui::RichText::new("キャッシュ率推移 (%)").strong());
                let raw_cache_pts: Vec<[f64; 2]> = analysis
                    .turns_history
                    .iter()
                    .enumerate()
                    .map(|(idx, t)| [idx as f64 + 1.0, t.cache_hit_percent()])
                    .collect();

                Plot::new("cache_history_plot")
                    .height(plot_h)
                    .width(plot_w)
                    .allow_scroll(false)
                    .allow_zoom(false)
                    .allow_drag(false)
                    .include_y(0.0)
                    .include_y(100.0)
                    .legend(Legend::default().position(Corner::LeftBottom))
                    .show(ui, |plot_ui| {
                        plot_ui.line(
                            Line::new("キャッシュ率 %", PlotPoints::from(raw_cache_pts.clone()))
                                .color(palette.cache_line)
                                .width(2.0),
                        );
                        if !raw_cache_pts.is_empty() {
                            plot_ui.points(
                                Points::new("", PlotPoints::from(raw_cache_pts))
                                    .color(palette.cache_line)
                                    .radius(3.0),
                            );
                        }
                    });
            });
        };

        if side_by_side {
            ui.horizontal(|ui| {
                render_ctx_plot(ui);
                render_cache_plot(ui);
            });
        } else {
            render_ctx_plot(ui);
            ui.add_space(8.0);
            render_cache_plot(ui);
        }
    }

    fn render_bottom_details(
        &mut self,
        ui: &mut egui::Ui,
        analysis: &ConversationAnalysis,
        palette: &Palette,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut self.detail_tab,
                DetailTab::Steps,
                format!("ステップ明細 ({})", analysis.step_summaries.len()),
            );
            ui.selectable_value(
                &mut self.detail_tab,
                DetailTab::HeavyTools,
                format!("重いツール Top 15 ({})", analysis.total_tool_calls),
            );
            if self.detail_tab == DetailTab::Steps {
                ui.separator();
                ui.selectable_value(&mut self.steps_newest_first, true, "最新順");
                ui.selectable_value(&mut self.steps_newest_first, false, "昇順");
                let total_steps = analysis.step_summaries.len();
                if total_steps > 200 {
                    let cap_note = if self.steps_newest_first {
                        format!("表示: 最新 200 / {total_steps} 件")
                    } else {
                        format!("表示: 先頭 200 / {total_steps} 件")
                    };
                    ui.label(
                        egui::RichText::new(cap_note)
                            .small()
                            .color(palette.text_muted),
                    );
                }
            }
        });
        ui.separator();

        match self.detail_tab {
            DetailTab::Steps => {
                if analysis.step_summaries.is_empty() {
                    ui.label(
                        egui::RichText::new("ステップ記録はありません").color(palette.text_muted),
                    );
                } else {
                    egui::ScrollArea::both()
                        .id_salt("steps_table_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui::Grid::new("steps_table")
                                .striped(true)
                                .num_columns(10)
                                .spacing([10.0, 4.0])
                                .show(ui, |ui| {
                                    ui.strong("Step#");
                                    ui.strong("種別");
                                    ui.strong("時刻");
                                    ui.strong("Prompt");
                                    ui.strong("Cached");
                                    ui.strong("Uncached");
                                    ui.strong("Hit%");
                                    ui.strong("Output");
                                    ui.strong("Compaction");
                                    ui.strong("概要");
                                    ui.end_row();

                                    let rows: Box<dyn Iterator<Item = &StepSummary>> =
                                        if self.steps_newest_first {
                                            Box::new(analysis.step_summaries.iter().rev().take(200))
                                        } else {
                                            Box::new(analysis.step_summaries.iter().take(200))
                                        };

                                    for st in rows {
                                        ui.monospace(st.step_index.to_string());
                                        ui.label(&st.step_type);
                                        ui.monospace(format_step_time(st.created_at.as_deref()));
                                        ui.monospace(format_tokens(st.prompt_tokens));
                                        ui.monospace(format_tokens(st.cached_tokens));
                                        ui.monospace(format_tokens(st.uncached_tokens));
                                        let hit_text = if st.prompt_tokens > 0 {
                                            format!("{:.1}%", st.cache_hit_percent())
                                        } else {
                                            "-".to_string()
                                        };
                                        ui.monospace(hit_text);
                                        ui.monospace(format_tokens(st.output_tokens));
                                        if st.compacted {
                                            ui.label(
                                                egui::RichText::new("圧縮")
                                                    .strong()
                                                    .color(palette.warn),
                                            );
                                        } else {
                                            ui.label(
                                                egui::RichText::new("-").color(palette.text_muted),
                                            );
                                        }
                                        ui.label(
                                            egui::RichText::new(truncate_ui(&st.summary, 120))
                                                .color(palette.text_muted),
                                        );
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
            DetailTab::HeavyTools => {
                if analysis.heavy_tool_calls.is_empty() {
                    ui.label(
                        egui::RichText::new("ツール呼び出し記録はありません")
                            .color(palette.text_muted),
                    );
                } else {
                    egui::ScrollArea::both()
                        .id_salt("heavy_tools_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui::Grid::new("heavy_tools_table")
                                .striped(true)
                                .num_columns(5)
                                .spacing([12.0, 4.0])
                                .show(ui, |ui| {
                                    ui.strong("#");
                                    ui.strong("ステップ");
                                    ui.strong("ツール");
                                    ui.strong("推定トークン");
                                    ui.strong("対象パス");
                                    ui.end_row();

                                    for (idx, tc) in analysis.heavy_tool_calls.iter().enumerate() {
                                        ui.monospace((idx + 1).to_string());
                                        ui.monospace(tc.step_index.to_string());
                                        ui.label(&tc.label);
                                        ui.monospace(format_tokens(tc.tokens));
                                        ui.label(
                                            egui::RichText::new(
                                                tc.target_path.as_deref().unwrap_or("-"),
                                            )
                                            .color(palette.text_muted),
                                        );
                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
        }
    }

    fn render_compact_hud(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        snap: &MonitorSnapshot,
        palette: &Palette,
    ) {
        egui::CentralPanel::default().show(ui, |ui| {
            let summary = snap
                .selected_id
                .as_ref()
                .and_then(|id| snap.index.by_id.get(id));
            let title = summary
                .map(|s| s.display_title.as_str())
                .or(snap.selected_id.as_deref())
                .unwrap_or("待機中");

            // Row 1: Title / Model / Always on Top / Normal mode
            // Right-side controls are placed via Sides::shrink_left so long titles never push buttons off-screen.
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(title).strong()).truncate());
                    if let Some(a) = &snap.analysis {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&a.model.model_label)
                                    .small()
                                    .color(palette.accent),
                            )
                            .truncate(),
                        );
                    }
                },
                |ui| {
                    if ui.small_button("通常").clicked() {
                        self.toggle_compact_mode(ctx);
                    }
                    if ui.selectable_label(self.always_on_top, "最前面").clicked() {
                        self.toggle_always_on_top(ctx);
                    }
                },
            );

            ui.add_space(6.0);

            // Row 2: Context usage bar + token counts
            if let Some(a) = &snap.analysis {
                draw_threshold_bar(ui, a.used_percent, palette, 14.0);
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let col = palette.usage_color(a.used_percent);
                    ui.label(
                        egui::RichText::new(format!("{:.1}%", a.used_percent))
                            .strong()
                            .color(col),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            format_tokens(a.used_tokens),
                            format_tokens(a.context_limit)
                        ))
                        .monospace()
                        .color(palette.text_muted),
                    );
                });

                ui.add_space(6.0);

                // Row 3: Latest cache / Cumulative cache / Compactions / Updated time
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("直近 {:.1}%", a.cache.cache_hit_ratio))
                            .small()
                            .color(palette.cache_line),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "累計 {:.1}%",
                            a.cache.session_cache_hit_ratio
                        ))
                        .small()
                        .color(palette.accent),
                    );
                    ui.label(
                        egui::RichText::new(format!("圧縮 {}回", a.compactions.len()))
                            .small()
                            .color(palette.text_muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(ts) = snap.last_updated_at {
                            let local_ts: DateTime<Local> = ts.with_timezone(&Local);
                            ui.label(
                                egui::RichText::new(local_ts.format("%H:%M:%S").to_string())
                                    .small()
                                    .monospace()
                                    .color(palette.text_muted),
                            );
                        }
                        if snap.db_busy {
                            ui.label(egui::RichText::new("ロック").small().color(palette.warn));
                        }
                    });
                });
            } else {
                ui.label(egui::RichText::new("セッションなし").color(palette.text_muted));
            }
        });
    }
}

fn kpi_frame(
    ui: &mut egui::Ui,
    width: f32,
    palette: &Palette,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::group(ui.style())
        .fill(palette.bg_card)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .show(ui, |ui| {
            ui.set_width(width - 14.0);
            ui.vertical(add_contents);
        });
}

fn draw_threshold_bar(ui: &mut egui::Ui, used_percent: f64, palette: &Palette, height: f32) {
    let w = ui.available_width().max(40.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, palette.bg_subtle);

    let frac = (used_percent / 100.0).clamp(0.0, 1.0) as f32;
    let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(w * frac, height));
    painter.rect_filled(fill_rect, 3.0, palette.usage_color(used_percent));

    // 70% and 85% threshold tick lines
    for pct in [0.70_f32, 0.85_f32] {
        let tx = rect.left() + w * pct;
        painter.line_segment(
            [egui::pos2(tx, rect.top()), egui::pos2(tx, rect.bottom())],
            egui::Stroke::new(1.0, palette.text_muted),
        );
    }
}

fn session_matches_query(s: &SessionSummary, q: &str) -> bool {
    s.display_title.to_lowercase().contains(q)
        || s.conversation_id.to_lowercase().contains(q)
        || s.project_label.to_lowercase().contains(q)
        || s.agent_name.to_lowercase().contains(q)
        || s.model_label.to_lowercase().contains(q)
        || format_model_short_label(&s.model_label)
            .to_lowercase()
            .contains(q)
}

fn truncate_ui(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max_chars).collect();
        s.push('…');
        s
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_mtime_short(ms: i64) -> String {
    if ms <= 0 {
        return "-".to_string();
    }
    let Some(dt_utc) = Utc.timestamp_millis_opt(ms).single() else {
        return "-".to_string();
    };
    let local: DateTime<Local> = dt_utc.with_timezone(&Local);
    local.format("%m/%d %H:%M").to_string()
}

fn format_step_time(raw: Option<&str>) -> String {
    let Some(s) = raw else {
        return "-".to_string();
    };
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        let local: DateTime<Local> = dt.with_timezone(&Local);
        return local.format("%H:%M:%S").to_string();
    }
    "-".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agy_core::analyzer::{
        CacheStats, CompactionEvent, ContextBuckets, HeavyToolCall, StepSummary, TurnPoint,
    };
    use agy_core::models::ModelInfo;
    use agy_core::paths::Surface;
    use agy_core::sessions::{ProjectInfo, SessionIndex, SessionTreeNode};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn sample_snapshot() -> MonitorSnapshot {
        let conv_id = "11111111-2222-3333-4444-555555555555".to_string();
        let sub_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string();
        let root_sum = SessionSummary {
            conversation_id: conv_id.clone(),
            surface: Surface::App,
            app_data_dir: PathBuf::from("."),
            title: "非常に長い日本語の会話タイトルでコンパクト表示時の見切れを検証するテスト"
                .to_string(),
            preview: String::new(),
            display_title:
                "非常に長い日本語の会話タイトルでコンパクト表示時の見切れを検証するテスト"
                    .to_string(),
            agent_name: String::new(),
            parent_conversation_id: String::new(),
            root_session_id: conv_id.clone(),
            root_title: "非常に長い日本語の会話タイトルでコンパクト表示時の見切れを検証するテスト"
                .to_string(),
            is_subagent: false,
            nesting_depth: 0,
            project_id: "p1".to_string(),
            project_key: "p1".to_string(),
            project_label: "agy-monitor".to_string(),
            workspace_keys: vec![],
            last_modified_ms: 1_700_000_000_000,
            effective_mtime_ms: 1_700_000_000_000,
            has_transcript: true,
            has_db: true,
            model_label: "Claude Opus 4.6 (High)".to_string(),
        };
        let sub_sum = SessionSummary {
            conversation_id: sub_id.clone(),
            display_title: "research".to_string(),
            agent_name: "research".to_string(),
            parent_conversation_id: conv_id.clone(),
            is_subagent: true,
            nesting_depth: 1,
            model_label: "Gemini Flash Worker".to_string(),
            ..root_sum.clone()
        };

        let mut by_id = std::collections::HashMap::new();
        by_id.insert(conv_id.clone(), root_sum.clone());
        by_id.insert(sub_id.clone(), sub_sum.clone());

        let index = SessionIndex {
            projects: vec![ProjectInfo {
                key: "p1".to_string(),
                label: "agy-monitor".to_string(),
            }],
            roots: vec![SessionTreeNode {
                summary: root_sum,
                children: vec![sub_sum],
            }],
            by_id,
            db_busy: false,
        };

        let analysis = ConversationAnalysis {
            conversation_id: conv_id.clone(),
            found: true,
            exact: true,
            model: ModelInfo {
                model_id: "claude-opus-4-6".to_string(),
                model_label: "Claude Opus 4.6 (High)".to_string(),
                context_limit: 256_000,
                model_max_tokens: 1_000_000,
                limit_source: "antigravity-default".to_string(),
            },
            context_limit: 256_000,
            used_tokens: 192_000,
            free_tokens: 64_000,
            used_percent: 75.0,
            step_count: 4,
            planner_turn_count: 3,
            last_step_at: Some("2026-11-22T15:00:00Z".to_string()),
            cache: CacheStats {
                last_prompt_tokens: 190_000,
                since_last_cache_tokens: 10_000,
                cached_tokens: 180_000,
                cache_hit_ratio: 94.7,
                last_output_tokens: 2_000,
                cumulative_input_tokens: 400_000,
                cumulative_cached_tokens: 360_000,
                session_cache_hit_ratio: 90.0,
                cumulative_output_tokens: 5_000,
            },
            compactions: vec![
                CompactionEvent {
                    step_index: 2,
                    before_tokens: 120_000,
                    after_tokens: Some(40_000),
                    saved_tokens: 80_000,
                    summary_tokens: 1_200,
                    created_at: None,
                    is_explicit: true,
                },
                CompactionEvent {
                    step_index: 5,
                    before_tokens: 150_000,
                    after_tokens: Some(50_000),
                    saved_tokens: 100_000,
                    summary_tokens: 0,
                    created_at: None,
                    is_explicit: false,
                },
            ],
            total_saved_tokens: 180_000,
            buckets: ContextBuckets {
                user_messages: 10_000,
                agent_replies: 20_000,
                agent_thinking: 15_000,
                tool_call_args: 5_000,
                tool_call_outputs: 100_000,
                system_tools: 4_800,
                system_prompt: 30_000,
                system_messages: 2_200,
                checkpoint_buffer: 5_000,
                free_space: 64_000,
                customization_breakdown: None,
            },
            turns_history: vec![
                TurnPoint {
                    step_index: 1,
                    prompt_tokens: 120_000,
                    uncached_tokens: 20_000,
                    cached_tokens: 100_000,
                    output_tokens: 1_000,
                    created_at: None,
                    compacted: false,
                },
                TurnPoint {
                    step_index: 3,
                    prompt_tokens: 40_000,
                    uncached_tokens: 5_000,
                    cached_tokens: 35_000,
                    output_tokens: 1_000,
                    created_at: None,
                    compacted: true,
                },
                TurnPoint {
                    step_index: 6,
                    prompt_tokens: 190_000,
                    uncached_tokens: 10_000,
                    cached_tokens: 180_000,
                    output_tokens: 2_000,
                    created_at: None,
                    compacted: true,
                },
            ],
            heavy_tool_calls: vec![HeavyToolCall {
                step_index: 1,
                tool_name: "read_file".to_string(),
                label: "read_file (app.rs)".to_string(),
                target_path: Some("C:\\repo\\app.rs".to_string()),
                tokens: 50_000,
            }],
            total_tool_calls: 1,
            step_summaries: vec![
                StepSummary {
                    step_index: 0,
                    step_type: "USER_INPUT".to_string(),
                    source: "USER".to_string(),
                    status: "DONE".to_string(),
                    created_at: Some("2026-11-22T15:00:00Z".to_string()),
                    prompt_tokens: 0,
                    uncached_tokens: 0,
                    cached_tokens: 0,
                    output_tokens: 0,
                    est_tokens: 100,
                    compacted: false,
                    summary: "init-step-0".to_string(),
                },
                StepSummary {
                    step_index: 1,
                    step_type: "PLANNER_RESPONSE".to_string(),
                    source: "MODEL".to_string(),
                    status: "DONE".to_string(),
                    created_at: Some("2026-11-22T15:01:00Z".to_string()),
                    prompt_tokens: 120_000,
                    uncached_tokens: 20_000,
                    cached_tokens: 100_000,
                    output_tokens: 1_000,
                    est_tokens: 120_000,
                    compacted: false,
                    summary: "plan-step-1".to_string(),
                },
                StepSummary {
                    step_index: 2,
                    step_type: "CHECKPOINT".to_string(),
                    source: "SYSTEM".to_string(),
                    status: "DONE".to_string(),
                    created_at: Some("2026-11-22T15:02:00Z".to_string()),
                    prompt_tokens: 121_000,
                    uncached_tokens: 121_000,
                    cached_tokens: 0,
                    output_tokens: 1_200,
                    est_tokens: 1_200,
                    compacted: true,
                    summary: "checkpoint-step-2".to_string(),
                },
                StepSummary {
                    step_index: 3,
                    step_type: "PLANNER_RESPONSE".to_string(),
                    source: "MODEL".to_string(),
                    status: "DONE".to_string(),
                    created_at: Some("2026-11-22T15:03:00Z".to_string()),
                    prompt_tokens: 190_000,
                    uncached_tokens: 10_000,
                    cached_tokens: 180_000,
                    output_tokens: 2_000,
                    est_tokens: 190_000,
                    compacted: false,
                    summary: "done-step-3".to_string(),
                },
            ],
            spawned_subagents: vec![],
            db_busy: false,
        };

        MonitorSnapshot {
            index,
            selected_id: Some(conv_id),
            analysis: Some(analysis),
            follow_latest: true,
            paused: false,
            last_updated_at: Some(Utc::now()),
            last_error: None,
            db_busy: false,
            analysis_count: 1,
            usage_report: None,
            usage_loading: false,
            customizations: None,
            customizations_loading: false,
        }
    }

    fn make_test_app() -> (TempDir, AgyMonitorApp) {
        let tmp = TempDir::new().unwrap();
        let paths = GeminiPaths::from_gemini_dir(tmp.path());
        let mut config = MonitorConfig::new(paths);
        config.poll_interval = Duration::from_secs(60);
        let monitor = MonitorHandle::start(config);
        let app = AgyMonitorApp {
            monitor,
            main_tab: MainTab::Realtime,
            detail_tab: DetailTab::Steps,
            steps_newest_first: true,
            theme_mode: ThemeMode::Dark,
            always_on_top: false,
            compact_mode: false,
            saved_normal_size: DEFAULT_NORMAL_SIZE,
            project_filter: String::new(),
            search_query: String::new(),
            expanded_roots: HashSet::new(),
            copied_until: None,
            usage_view_state: UsageViewState::default(),
            phase2_initialized: false,
        };
        (tmp, app)
    }

    #[test]
    fn gui_02_compact_hud_keeps_controls_within_380px() {
        let (_tmp, mut app) = make_test_app();
        app.compact_mode = true;
        let snap = sample_snapshot();
        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, COMPACT_SIZE)),
            ..Default::default()
        };

        let mut used_rect = egui::Rect::NOTHING;
        let mut output = ctx.run_ui(raw_input, |ui| {
            app.render_ui(ui, &snap);
            used_rect = ui.min_rect();
        });
        let shapes_len = output.shapes.len();
        output.textures_delta.clear();

        assert!(
            used_rect.right() <= COMPACT_SIZE.x + 1.0,
            "compact HUD overflowed horizontally: right={} > {}",
            used_rect.right(),
            COMPACT_SIZE.x
        );
        assert!(
            used_rect.bottom() <= COMPACT_SIZE.y + 1.0,
            "compact HUD overflowed vertically: bottom={} > {}",
            used_rect.bottom(),
            COMPACT_SIZE.y
        );
        assert!(shapes_len > 0);
    }

    #[test]
    fn gui_03_normal_narrow_and_tabs_render_cleanly() {
        let (_tmp, mut app) = make_test_app();
        let snap = sample_snapshot();
        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);

        // Render at minimum window size (760x520) across tabs, sort orders & themes
        for (tab, detail, newest_first, theme) in [
            (MainTab::Realtime, DetailTab::Steps, true, ThemeMode::Dark),
            (MainTab::Realtime, DetailTab::Steps, false, ThemeMode::Light),
            (
                MainTab::Realtime,
                DetailTab::HeavyTools,
                true,
                ThemeMode::Light,
            ),
            (
                MainTab::UsagePhase2,
                DetailTab::Steps,
                true,
                ThemeMode::Dark,
            ),
        ] {
            app.main_tab = tab;
            app.detail_tab = detail;
            app.steps_newest_first = newest_first;
            app.theme_mode = theme;
            apply_theme(&ctx, theme);

            for _ in 0..2 {
                let raw_input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 520.0),
                    )),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(raw_input, |ui| {
                    app.render_ui(ui, &snap);
                });
                let shapes_len = output.shapes.len();
                assert!(
                    output
                        .shapes
                        .iter()
                        .all(|s| s.clip_rect.right() <= 760.5 && s.clip_rect.bottom() <= 520.5),
                    "narrow layout emitted shapes clipped beyond viewport"
                );
                output.textures_delta.clear();
                assert!(shapes_len > 0);
            }
        }
    }

    fn collect_rendered_texts(
        shapes: &[egui::epaint::ClippedShape],
    ) -> Vec<(String, egui::Rect, egui::Rect)> {
        fn walk_shape(
            shape: &egui::epaint::Shape,
            clip_rect: egui::Rect,
            out: &mut Vec<(String, egui::Rect, egui::Rect)>,
        ) {
            match shape {
                egui::epaint::Shape::Text(ts) => {
                    let rect = egui::Rect::from_min_size(ts.pos, ts.galley.size());
                    out.push((ts.galley.text().to_string(), rect, clip_rect));
                }
                egui::epaint::Shape::Vec(children) => {
                    for child in children {
                        walk_shape(child, clip_rect, out);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for clipped in shapes {
            walk_shape(&clipped.shape, clipped.clip_rect, &mut out);
        }
        out
    }

    #[test]
    fn gui_04_steps_table_sort_toggle_and_columns() {
        let (_tmp, mut app) = make_test_app();
        let snap = sample_snapshot();
        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        apply_theme(&ctx, ThemeMode::Dark);

        let viewport = egui::vec2(1400.0, 1400.0);
        let run_frame = |app: &mut AgyMonitorApp, s: &MonitorSnapshot, events: Vec<egui::Event>| {
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(raw_input, |ui| {
                app.render_ui(ui, s);
            });
            let texts = collect_rendered_texts(&output.shapes);
            let asc_pos = texts
                .iter()
                .find(|(txt, _, _)| txt == "昇順")
                .map(|(_, rect, _)| rect.center());
            let desc_pos = texts
                .iter()
                .find(|(txt, _, _)| txt == "最新順")
                .map(|(_, rect, _)| rect.center());
            let mut output = output;
            output.textures_delta.clear();
            (texts, asc_pos, desc_pos)
        };

        // Frame 1 & 2 (allow layout to settle): default newest-first ("最新順")
        let _ = run_frame(&mut app, &snap, vec![]);
        let (texts_desc, asc_pos, _desc_pos) = run_frame(&mut app, &snap, vec![]);
        assert!(app.steps_newest_first);

        // Verify all 10 column headers, sort toggles, and new cell values rendered completely unclipped
        for required in [
            "最新順",
            "昇順",
            "Step#",
            "種別",
            "時刻",
            "Prompt",
            "Cached",
            "Uncached",
            "Hit%",
            "Output",
            "Compaction",
            "概要",
            "20.0k", // Uncached on step 1
            "83.3%", // Hit% on step 1 (100k / 120k)
            "圧縮",  // Compaction badge on step 2
        ] {
            let found = texts_desc.iter().find(|(t, rect, clip)| {
                t == required
                    && rect.left() >= clip.left() - 0.5
                    && rect.right() <= clip.right() + 0.5
                    && rect.top() >= clip.top() - 0.5
                    && rect.bottom() <= clip.bottom() + 0.5
            });
            assert!(
                found.is_some(),
                "expected '{required}' to be rendered fully unclipped in Steps table; texts={:?}",
                texts_desc.iter().map(|(t, _, _)| t).collect::<Vec<_>>()
            );
        }

        let y_step0_desc = texts_desc
            .iter()
            .find(|(t, _, _)| t == "init-step-0")
            .expect("init-step-0 rendered")
            .1
            .top();
        let y_step3_desc = texts_desc
            .iter()
            .find(|(t, _, _)| t == "done-step-3")
            .expect("done-step-3 rendered")
            .1
            .top();
        assert!(
            y_step3_desc < y_step0_desc,
            "newest-first must place step 3 ({y_step3_desc}) above step 0 ({y_step0_desc})"
        );

        // Click the "昇順" toggle button via pointer events
        let click_pos = asc_pos.expect("昇順 button position");
        let _ = run_frame(
            &mut app,
            &snap,
            vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(
            !app.steps_newest_first,
            "clicking '昇順' must switch steps_newest_first to false"
        );

        let (texts_asc, _, desc_pos) = run_frame(&mut app, &snap, vec![]);
        let y_step0_asc = texts_asc
            .iter()
            .find(|(t, _, _)| t == "init-step-0")
            .expect("init-step-0 rendered in asc")
            .1
            .top();
        let y_step3_asc = texts_asc
            .iter()
            .find(|(t, _, _)| t == "done-step-3")
            .expect("done-step-3 rendered in asc")
            .1
            .top();
        assert!(
            y_step0_asc < y_step3_asc,
            "ascending sort must place step 0 ({y_step0_asc}) above step 3 ({y_step3_asc})"
        );

        // Click "最新順" back to true and verify order flips back
        let click_desc = desc_pos.expect("最新順 button position");
        let _ = run_frame(
            &mut app,
            &snap,
            vec![
                egui::Event::PointerMoved(click_desc),
                egui::Event::PointerButton {
                    pos: click_desc,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: click_desc,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(
            app.steps_newest_first,
            "clicking '最新順' must switch steps_newest_first back to true"
        );
        let (texts_back, _, _) = run_frame(&mut app, &snap, vec![]);
        let y_step0_back = texts_back
            .iter()
            .find(|(t, _, _)| t == "init-step-0")
            .unwrap()
            .1
            .top();
        let y_step3_back = texts_back
            .iter()
            .find(|(t, _, _)| t == "done-step-3")
            .unwrap()
            .1
            .top();
        assert!(y_step3_back < y_step0_back);

        // >200 steps cap notice & slice boundaries in both sort orders
        let mut snap_many = sample_snapshot();
        if let Some(a) = snap_many.analysis.as_mut() {
            a.step_summaries = (0..205)
                .map(|i| StepSummary {
                    step_index: i,
                    step_type: "PLANNER_RESPONSE".to_string(),
                    source: "MODEL".to_string(),
                    status: "DONE".to_string(),
                    created_at: None,
                    prompt_tokens: 1000,
                    uncached_tokens: 100,
                    cached_tokens: 900,
                    output_tokens: 50,
                    est_tokens: 1000,
                    compacted: false,
                    summary: format!("bulk-step-{i}"),
                })
                .collect();
            a.step_count = 205;
        }
        app.steps_newest_first = true;
        let (texts_many_desc, _, _) = run_frame(&mut app, &snap_many, vec![]);
        assert!(
            texts_many_desc
                .iter()
                .any(|(t, _, _)| t == "表示: 最新 200 / 205 件")
        );
        assert!(texts_many_desc.iter().any(|(t, _, _)| t == "bulk-step-204"));
        assert!(!texts_many_desc.iter().any(|(t, _, _)| t == "bulk-step-0"));

        app.steps_newest_first = false;
        let (texts_many_asc, _, _) = run_frame(&mut app, &snap_many, vec![]);
        assert!(
            texts_many_asc
                .iter()
                .any(|(t, _, _)| t == "表示: 先頭 200 / 205 件")
        );
        assert!(texts_many_asc.iter().any(|(t, _, _)| t == "bulk-step-0"));
        assert!(!texts_many_asc.iter().any(|(t, _, _)| t == "bulk-step-204"));

        // Empty step_summaries renders empty-state label without panic
        let mut snap_empty = sample_snapshot();
        if let Some(a) = snap_empty.analysis.as_mut() {
            a.step_summaries.clear();
            a.step_count = 0;
        }
        let (texts_empty, _, _) = run_frame(&mut app, &snap_empty, vec![]);
        assert!(
            texts_empty
                .iter()
                .any(|(t, _, _)| t == "ステップ記録はありません")
        );
    }

    fn sample_snapshot_with_60_steps() -> MonitorSnapshot {
        let mut snap = sample_snapshot();
        if let Some(a) = snap.analysis.as_mut() {
            a.step_summaries = (0..60)
                .map(|i| StepSummary {
                    step_index: i,
                    step_type: "PLANNER_RESPONSE".to_string(),
                    source: "MODEL".to_string(),
                    status: "DONE".to_string(),
                    created_at: Some("2026-11-22T15:01:00Z".to_string()),
                    prompt_tokens: 1000,
                    uncached_tokens: 100,
                    cached_tokens: 900,
                    output_tokens: 50,
                    est_tokens: 1000,
                    compacted: false,
                    summary: format!("scroll-step-{i}"),
                })
                .collect();
            a.step_count = 60;
        }
        snap
    }

    #[test]
    fn gui_06_step_scroll_keeps_monitor_and_plots_fixed() {
        let (_tmp, mut app) = make_test_app();
        let snap = sample_snapshot_with_60_steps();

        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        apply_theme(&ctx, ThemeMode::Dark);

        let viewport = egui::vec2(1240.0, 820.0);
        let run_frame = |app: &mut AgyMonitorApp, events: Vec<egui::Event>| {
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw_input, |ui| {
                app.render_ui(ui, &snap);
            });
            let texts = collect_rendered_texts(&output.shapes);
            output.textures_delta.clear();
            texts
        };

        let _ = run_frame(&mut app, vec![]);
        let before = run_frame(&mut app, vec![]);

        let find_y = |texts: &[(String, egui::Rect, egui::Rect)], needle: &str| {
            texts
                .iter()
                .find(|(t, _, _)| t == needle)
                .unwrap_or_else(|| panic!("missing text '{needle}'"))
                .1
                .top()
        };

        let title = snap.index.by_id[snap.selected_id.as_ref().unwrap()]
            .display_title
            .as_str();
        let header_y_before = find_y(&before, title);
        let kpi_y_before = find_y(&before, "コンテキスト使用率");
        let plot_y_before = find_y(&before, "コンテキスト推移");
        let step59_rect = before
            .iter()
            .find(|(t, _, _)| t == "scroll-step-59")
            .expect("scroll-step-59 visible initially")
            .1;
        let step59_y_before = step59_rect.top();

        // Scroll downward with pointer positioned inside the STEP table
        let hover_pos = step59_rect.center();
        let _ = run_frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(hover_pos),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -240.0),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let after = run_frame(&mut app, vec![]);

        assert!(
            (find_y(&after, title) - header_y_before).abs() < 0.1,
            "session header moved during STEP scroll"
        );
        assert!(
            (find_y(&after, "コンテキスト使用率") - kpi_y_before).abs() < 0.1,
            "KPI card moved during STEP scroll"
        );
        assert!(
            (find_y(&after, "コンテキスト推移") - plot_y_before).abs() < 0.1,
            "plot heading moved during STEP scroll"
        );

        let step59_y_after = after
            .iter()
            .find(|(t, _, _)| t == "scroll-step-59")
            .map(|(_, r, _)| r.top());
        assert!(
            step59_y_after.is_none_or(|y| y < step59_y_before - 20.0),
            "expected STEP rows to scroll upward (before={step59_y_before}, after={step59_y_after:?})"
        );
    }

    #[test]
    fn gui_07_model_label_right_of_session_titles() {
        let (_tmp, mut app) = make_test_app();
        let mut snap = sample_snapshot();
        let root_id = snap.selected_id.clone().unwrap();
        let short_title = "セッションタイトル検証";
        if let Some(s) = snap.index.by_id.get_mut(&root_id) {
            s.display_title = short_title.to_string();
        }
        if let Some(node) = snap.index.roots.first_mut() {
            node.summary.display_title = short_title.to_string();
        }
        app.expanded_roots.insert(root_id);

        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        apply_theme(&ctx, ThemeMode::Dark);

        let viewport = egui::vec2(1240.0, 820.0);
        let run_frame = |app: &mut AgyMonitorApp, s: &MonitorSnapshot| {
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw_input, |ui| {
                app.render_ui(ui, s);
            });
            let texts = collect_rendered_texts(&output.shapes);
            output.textures_delta.clear();
            texts
        };

        let _ = run_frame(&mut app, &snap);
        let texts = run_frame(&mut app, &snap);

        // 1. Right pane header: single-line model label immediately right of title and before IDコピー
        let header_title = texts
            .iter()
            .find(|(t, r, _)| t == short_title && r.left() > 280.0)
            .expect("header title in main pane")
            .1;
        let header_model = texts
            .iter()
            .find(|(t, r, _)| t == "Claude Opus 4.6 (High)" && r.left() > 280.0)
            .expect("model label badge in header")
            .1;
        let id_copy = texts
            .iter()
            .find(|(t, r, _)| t == "IDコピー" && r.left() > 280.0)
            .expect("IDコピー button in header")
            .1;
        assert!(
            header_model.left() >= header_title.right() - 1.0,
            "model badge ({header_model:?}) must be right of title ({header_title:?})"
        );
        assert!(
            (header_model.center().y - header_title.center().y).abs() < 4.0,
            "model badge ({header_model:?}) must sit on same line as title ({header_title:?})"
        );
        assert!(
            header_model.right() <= id_copy.left() + 1.0,
            "model badge ({header_model:?}) must precede IDコピー ({id_copy:?})"
        );

        // 2. Sidebar root & subagent rows include short model label in their rendered LayoutJob text
        let sidebar_root = texts
            .iter()
            .find(|(t, r, _)| r.left() < 280.0 && t.contains("Claude Opus 4.6 (H)"))
            .expect("root sidebar entry should contain short model label");
        assert!(sidebar_root.0.contains(short_title));

        let sidebar_sub = texts
            .iter()
            .find(|(t, r, _)| {
                r.left() < 280.0 && t.contains("↳ research") && t.contains("Flash Worker")
            })
            .expect("subagent sidebar entry should contain short model label");
        assert!(sidebar_sub.0.find("↳ research") < sidebar_sub.0.find("Flash Worker"));

        // 3. Even with a long title, the model badge stays single-line (never breaks mid-badge)
        let long_snap = sample_snapshot();
        let _ = run_frame(&mut app, &long_snap);
        let long_texts = run_frame(&mut app, &long_snap);
        let long_badge = long_texts
            .iter()
            .find(|(t, r, _)| t == "Claude Opus 4.6 (High)" && r.left() > 280.0)
            .expect("long-title header must still render unbroken single-line model badge")
            .1;
        assert!(
            long_badge.height() <= 22.0,
            "model badge must stay single-line, got height={}",
            long_badge.height()
        );
    }

    #[test]
    fn gui_08_narrow_pane_reserves_detail_tabs_and_step_scroll() {
        let mut snap = sample_snapshot_with_60_steps();
        let root_id = snap.selected_id.clone().unwrap();
        let short_title = "狭幅ペイン検証";
        if let Some(s) = snap.index.by_id.get_mut(&root_id) {
            s.display_title = short_title.to_string();
        }
        if let Some(node) = snap.index.roots.first_mut() {
            node.summary.display_title = short_title.to_string();
        }

        for viewport in [egui::vec2(580.0, 620.0), egui::vec2(620.0, 660.0)] {
            let (_tmp, mut app) = make_test_app();
            let ctx = egui::Context::default();
            install_japanese_fonts(&ctx);
            apply_theme(&ctx, ThemeMode::Dark);

            let run_frame = |app: &mut AgyMonitorApp, events: Vec<egui::Event>| {
                let raw_input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                    events,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(raw_input, |ui| {
                    app.render_ui(ui, &snap);
                });
                let texts = collect_rendered_texts(&output.shapes);
                output.textures_delta.clear();
                texts
            };

            let _ = run_frame(&mut app, vec![]);
            let texts = run_frame(&mut app, vec![]);

            // Sidebar must be visible alongside the narrow right pane
            assert!(
                texts
                    .iter()
                    .any(|(t, r, _)| r.left() < 280.0 && t.contains("agy-monitor")),
                "expected sidebar visible at viewport {viewport:?}"
            );

            // Both detail tabs must fit within their clip rect (not pushed off the bottom)
            for tab_label in ["ステップ明細 (60)", "重いツール Top 15 (1)"] {
                let (_, rect, clip) = texts
                    .iter()
                    .find(|(t, _, _)| t == tab_label)
                    .unwrap_or_else(|| {
                        panic!("missing detail tab '{tab_label}' at viewport {viewport:?}")
                    });
                assert!(
                    rect.top() >= clip.top() - 0.5 && rect.bottom() <= clip.bottom() + 0.5,
                    "tab '{tab_label}' clipped at {viewport:?}: rect={rect:?}, clip={clip:?}"
                );
            }

            // On Steps tab, step rows must have >= 80.0pt visible area and scroll on MouseWheel
            let (_, step59_rect, step59_clip) = texts
                .iter()
                .find(|(t, r, _)| t == "59" && r.left() >= 280.0)
                .unwrap_or_else(|| panic!("expected Step# 59 visible at {viewport:?}"));
            assert!(
                step59_clip.height() >= 80.0,
                "steps area height too small at {viewport:?}: {}",
                step59_clip.height()
            );
            assert!(
                step59_rect.top() >= step59_clip.top() - 0.5
                    && step59_rect.bottom() <= step59_clip.bottom() + 0.5,
                "Step# 59 clipped at {viewport:?}: rect={step59_rect:?}, clip={step59_clip:?}"
            );
            let step59_y_before = step59_rect.top();

            let hover_pos = step59_rect.center();
            let _ = run_frame(
                &mut app,
                vec![
                    egui::Event::PointerMoved(hover_pos),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -240.0),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            let after_scroll = run_frame(&mut app, vec![]);
            let step59_y_after = after_scroll
                .iter()
                .find(|(t, r, _)| t == "59" && r.left() >= 280.0)
                .map(|(_, r, _)| r.top());
            assert!(
                step59_y_after.is_none_or(|y| y < step59_y_before - 20.0),
                "expected step rows to scroll at {viewport:?} (before={step59_y_before}, after={step59_y_after:?})"
            );

            // Click "重いツール Top 15 (1)" and verify detail_tab switches
            let heavy_pos = after_scroll
                .iter()
                .find(|(t, _, _)| t == "重いツール Top 15 (1)")
                .expect("重いツール Top 15 (1) tab position")
                .1
                .center();
            let _ = run_frame(
                &mut app,
                vec![
                    egui::Event::PointerMoved(heavy_pos),
                    egui::Event::PointerButton {
                        pos: heavy_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: heavy_pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            assert_eq!(
                app.detail_tab,
                DetailTab::HeavyTools,
                "clicking '重いツール Top 15 (1)' must switch detail_tab at {viewport:?}"
            );
        }
    }

    #[test]
    fn gui_09_sidebar_search_matches_short_model_labels() {
        let base_snap = sample_snapshot();
        let template = base_snap.index.roots[0].summary.clone();

        let make_summary =
            |id: &str, title: &str, project: &str, model_label: &str| SessionSummary {
                conversation_id: id.to_string(),
                title: title.to_string(),
                display_title: title.to_string(),
                root_session_id: id.to_string(),
                root_title: title.to_string(),
                project_id: project.to_string(),
                project_key: project.to_string(),
                project_label: project.to_string(),
                model_label: model_label.to_string(),
                ..template.clone()
            };

        let orch = make_summary(
            "conv-orch-111",
            "Alpha Architecture",
            "proj-alpha",
            "Orchestrator (Carbon)",
        );
        let flash = make_summary(
            "conv-flash-222",
            "Beta Subtask",
            "proj-beta",
            "Gemini Flash Worker",
        );
        let opus = make_summary(
            "conv-opus-333",
            "Gamma Deep Review",
            "proj-gamma",
            "Claude Opus 4.6 (High)",
        );

        // Unit checks on session_matches_query
        assert!(session_matches_query(&orch, "orch (carbon)"));
        assert!(!session_matches_query(&flash, "orch (carbon)"));
        assert!(!session_matches_query(&opus, "orch (carbon)"));

        assert!(session_matches_query(&flash, "flash worker"));
        assert!(!session_matches_query(&orch, "flash worker"));

        assert!(session_matches_query(&opus, "opus 4.6 (h)"));
        assert!(!session_matches_query(&orch, "opus 4.6 (h)"));

        assert!(session_matches_query(&orch, "orchestrator (carbon)"));
        assert!(session_matches_query(&orch, "alpha architecture"));
        assert!(session_matches_query(&flash, "conv-flash-222"));
        assert!(session_matches_query(&opus, "proj-gamma"));

        for s in [&orch, &flash, &opus] {
            assert!(!session_matches_query(s, "nonexistent-model"));
        }

        // UI sidebar search checks with mixed case & substrings
        let mut by_id = std::collections::HashMap::new();
        for s in [&orch, &flash, &opus] {
            by_id.insert(s.conversation_id.clone(), s.clone());
        }
        let mut snap = base_snap;
        snap.index = SessionIndex {
            projects: vec![],
            roots: vec![
                SessionTreeNode {
                    summary: orch,
                    children: vec![],
                },
                SessionTreeNode {
                    summary: flash,
                    children: vec![],
                },
                SessionTreeNode {
                    summary: opus,
                    children: vec![],
                },
            ],
            by_id,
            db_busy: false,
        };
        snap.selected_id = Some("conv-orch-111".to_string());
        if let Some(a) = snap.analysis.as_mut() {
            a.conversation_id = "conv-orch-111".to_string();
        }

        let (_tmp, mut app) = make_test_app();
        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        apply_theme(&ctx, ThemeMode::Dark);

        let run_search = |app: &mut AgyMonitorApp, query: &str| {
            app.search_query = query.to_string();
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1240.0, 820.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw_input, |ui| {
                app.render_ui(ui, &snap);
            });
            let texts = collect_rendered_texts(&output.shapes);
            output.textures_delta.clear();
            texts
        };

        let sidebar_has = |texts: &[(String, egui::Rect, egui::Rect)], needle: &str| {
            texts
                .iter()
                .any(|(t, r, _)| r.left() < 280.0 && t.contains(needle))
        };

        for (query, expected_title, hidden_titles) in [
            (
                "Orch (Carbon)",
                "Alpha Architecture",
                &["Beta Subtask", "Gamma Deep Review"][..],
            ),
            (
                "cArBoN",
                "Alpha Architecture",
                &["Beta Subtask", "Gamma Deep Review"][..],
            ),
            (
                "Flash Worker",
                "Beta Subtask",
                &["Alpha Architecture", "Gamma Deep Review"][..],
            ),
            (
                "Opus 4.6 (H)",
                "Gamma Deep Review",
                &["Alpha Architecture", "Beta Subtask"][..],
            ),
            (
                "4.6 (h)",
                "Gamma Deep Review",
                &["Alpha Architecture", "Beta Subtask"][..],
            ),
            (
                "Orchestrator (Carbon)",
                "Alpha Architecture",
                &["Beta Subtask", "Gamma Deep Review"][..],
            ),
            (
                "conv-flash-222",
                "Beta Subtask",
                &["Alpha Architecture", "Gamma Deep Review"][..],
            ),
            (
                "proj-gamma",
                "Gamma Deep Review",
                &["Alpha Architecture", "Beta Subtask"][..],
            ),
        ] {
            let texts = run_search(&mut app, query);
            assert!(
                sidebar_has(&texts, expected_title),
                "query {query:?} should match {expected_title:?}"
            );
            for hidden in hidden_titles {
                assert!(
                    !sidebar_has(&texts, hidden),
                    "query {query:?} should not match {hidden:?}"
                );
            }
        }

        let none_texts = run_search(&mut app, "nonexistent-model");
        assert!(sidebar_has(&none_texts, "一致するセッションがありません"));
        for title in ["Alpha Architecture", "Beta Subtask", "Gamma Deep Review"] {
            assert!(!sidebar_has(&none_texts, title));
        }
    }
}
