use agy_core::analyzer::CustomizationBreakdown;
use agy_core::customizations::{
    CustomizationInventory, CustomizationItem, CustomizationKind, LoadTiming,
};
use agy_core::usage::{DailyUsagePoint, UsageBreakdownRow, UsagePeriod, UsageReport};
use chrono::{Days, NaiveDate};
use eframe::egui;
use egui_plot::{Corner, HoverPosition, Legend, Line, Plot, PlotPoints, Points};

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase2SubTab {
    UsageStats,
    Customizations,
}

#[derive(Debug, Clone)]
pub struct UsageViewState {
    pub sub_tab: Phase2SubTab,
    pub selected_period: UsagePeriod,
    pub project_filter: String,
    pub customization_search: String,
    pub timing_filter: Option<LoadTiming>,
    pub kind_filter: Option<CustomizationKind>,
}

impl Default for UsageViewState {
    fn default() -> Self {
        Self {
            sub_tab: Phase2SubTab::UsageStats,
            selected_period: UsagePeriod::Last7Days,
            project_filter: String::new(),
            customization_search: String::new(),
            timing_filter: None,
            kind_filter: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UsageViewActions {
    pub refresh_usage: Option<UsagePeriod>,
    pub refresh_customizations: bool,
}

const ALL_PERIODS: [UsagePeriod; 4] = [
    UsagePeriod::Today,
    UsagePeriod::Last7Days,
    UsagePeriod::Last30Days,
    UsagePeriod::AllTime,
];

const ALL_KINDS: [CustomizationKind; 5] = [
    CustomizationKind::McpTool,
    CustomizationKind::McpInstruction,
    CustomizationKind::RuleFile,
    CustomizationKind::SkillIndex,
    CustomizationKind::CustomAgent,
];

pub fn render_usage_phase2(
    ui: &mut egui::Ui,
    state: &mut UsageViewState,
    usage_report: Option<&UsageReport>,
    usage_loading: bool,
    customizations: Option<&CustomizationInventory>,
    customizations_loading: bool,
    palette: &Palette,
) -> UsageViewActions {
    let mut actions = UsageViewActions::default();

    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(&mut state.sub_tab, Phase2SubTab::UsageStats, "期間統計");
        ui.selectable_value(
            &mut state.sub_tab,
            Phase2SubTab::Customizations,
            "カスタマイズ / MCP 棚卸し",
        );
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("usage_phase2_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| match state.sub_tab {
            Phase2SubTab::UsageStats => {
                render_usage_stats_view(
                    ui,
                    state,
                    usage_report,
                    usage_loading,
                    palette,
                    &mut actions,
                );
            }
            Phase2SubTab::Customizations => {
                render_customizations_view(
                    ui,
                    state,
                    customizations,
                    customizations_loading,
                    palette,
                    &mut actions,
                );
            }
        });

    actions
}

fn render_usage_stats_view(
    ui: &mut egui::Ui,
    state: &mut UsageViewState,
    usage_report: Option<&UsageReport>,
    usage_loading: bool,
    palette: &Palette,
    actions: &mut UsageViewActions,
) {
    ui.horizontal_wrapped(|ui| {
        for period in ALL_PERIODS {
            if ui
                .selectable_label(state.selected_period == period, period_label(period))
                .clicked()
            {
                state.selected_period = period;
                actions.refresh_usage = Some(period);
            }
        }

        ui.separator();

        if ui
            .add_enabled(!usage_loading, egui::Button::new("再集計"))
            .clicked()
        {
            actions.refresh_usage = Some(state.selected_period);
        }

        if usage_loading {
            ui.spinner();
            ui.label(
                egui::RichText::new("集計中...")
                    .small()
                    .color(palette.text_muted),
            );
        }
    });

    ui.add_space(6.0);

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("プロジェクト絞込:").color(palette.text_muted));
        let filter_w = (ui.available_width() - 120.0).clamp(80.0, 200.0);
        ui.add(
            egui::TextEdit::singleline(&mut state.project_filter)
                .id_salt("usage_project_filter_input")
                .hint_text("名称またはキー")
                .desired_width(filter_w),
        );

        if let Some(report) = usage_report
            && !report.by_project.is_empty()
        {
            let combo_w = (ui.available_width() - 50.0).clamp(80.0, 160.0);
            let selected_text = if state.project_filter.is_empty() {
                "すべて".to_string()
            } else {
                report
                    .by_project
                    .iter()
                    .find(|r| r.key == state.project_filter || r.label == state.project_filter)
                    .map(|r| truncate_ui(&r.label, 18))
                    .unwrap_or_else(|| truncate_ui(&state.project_filter, 18))
            };
            egui::ComboBox::from_id_salt("usage_project_selector")
                .width(combo_w)
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.project_filter, String::new(), "すべて");
                    for row in &report.by_project {
                        ui.selectable_value(&mut state.project_filter, row.key.clone(), &row.label);
                    }
                });
        }

        if !state.project_filter.is_empty() && ui.small_button("クリア").clicked() {
            state.project_filter.clear();
        }
    });

    let Some(report) = usage_report else {
        ui.add_space(12.0);
        let msg = if usage_loading {
            "利用統計を集計しています..."
        } else {
            "利用統計データがありません（「再集計」で取得できます）"
        };
        ui.label(egui::RichText::new(msg).color(palette.text_muted));
        return;
    };

    if report.truncated {
        ui.add_space(6.0);
        egui::Frame::new()
            .fill(palette.bg_subtle)
            .stroke(egui::Stroke::new(1.0, palette.warn))
            .corner_radius(4.0)
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                ui.set_width((ui.available_width()).max(60.0));
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "注意: 対象セッション数が上限を超えたため直近 {} / {} 件のみ集計しました",
                            report.analyzed_sessions, report.total_sessions
                        ))
                        .small()
                        .color(palette.warn),
                    );
                });
            });
    }

    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            egui::RichText::new(
                "※集計基準: 会話の最終更新日（ローカル日付）ごとにセッション累計を集計",
            )
            .small()
            .color(palette.text_muted),
        )
        .wrap(),
    );

    ui.add_space(8.0);
    render_usage_kpis(ui, report, palette);

    ui.add_space(10.0);
    render_daily_plot(ui, &report.daily, palette);

    ui.add_space(10.0);
    render_breakdown_table(
        ui,
        "モデル別内訳",
        "モデル",
        "usage_model_table",
        &report.by_model,
        "",
        palette,
    );

    ui.add_space(8.0);
    render_breakdown_table(
        ui,
        "プロジェクト別内訳",
        "プロジェクト",
        "usage_project_table",
        &report.by_project,
        &state.project_filter,
        palette,
    );

    ui.add_space(8.0);
    render_breakdown_table(
        ui,
        "サーフェス別内訳",
        "サーフェス",
        "usage_surface_table",
        &report.by_surface,
        "",
        palette,
    );
}

fn render_usage_kpis(ui: &mut egui::Ui, report: &UsageReport, palette: &Palette) {
    let avail = ui.available_width();
    let spacing = 8.0;
    let cols = if avail >= 580.0 { 4 } else { 2 };
    let card_w = ((avail - spacing * (cols as f32 - 1.0)) / cols as f32).max(90.0);

    let card_sessions = |ui: &mut egui::Ui| {
        kpi_card(ui, card_w, palette, |ui| {
            ui.label(
                egui::RichText::new("セッション数")
                    .small()
                    .color(palette.text_muted),
            );
            ui.label(
                egui::RichText::new(format!("{} 件", report.analyzed_sessions))
                    .size(20.0)
                    .strong()
                    .color(palette.text_primary),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "対象 {} / 除外 {}",
                        report.total_sessions, report.skipped_sessions
                    ))
                    .small()
                    .color(palette.text_muted),
                )
                .truncate(),
            );
        });
    };

    let card_tokens = |ui: &mut egui::Ui| {
        kpi_card(ui, card_w, palette, |ui| {
            ui.label(
                egui::RichText::new("累計入力 / 出力")
                    .small()
                    .color(palette.text_muted),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_tokens(report.total_input_tokens),
                        format_tokens(report.total_output_tokens)
                    ))
                    .size(18.0)
                    .strong()
                    .color(palette.accent),
                )
                .truncate(),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "キャッシュ {}",
                        format_tokens(report.total_cached_tokens)
                    ))
                    .small()
                    .color(palette.text_muted),
                )
                .truncate(),
            );
        });
    };

    let card_cache = |ui: &mut egui::Ui| {
        kpi_card(ui, card_w, palette, |ui| {
            ui.label(
                egui::RichText::new("平均キャッシュ率")
                    .small()
                    .color(palette.text_muted),
            );
            let pct = (report.cache_hit_ratio * 100.0).clamp(0.0, 100.0);
            ui.label(
                egui::RichText::new(format!("{pct:.1}%"))
                    .size(20.0)
                    .strong()
                    .color(palette.cache_line),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} / {}",
                        format_tokens(report.total_cached_tokens),
                        format_tokens(report.total_input_tokens)
                    ))
                    .small()
                    .color(palette.text_muted),
                )
                .truncate(),
            );
        });
    };

    let card_compactions = |ui: &mut egui::Ui| {
        kpi_card(ui, card_w, palette, |ui| {
            ui.label(
                egui::RichText::new("コンパクション合計")
                    .small()
                    .color(palette.text_muted),
            );
            ui.label(
                egui::RichText::new(format!("{} 回", report.total_compactions))
                    .size(20.0)
                    .strong()
                    .color(palette.ok),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(period_label(report.period))
                        .small()
                        .color(palette.text_muted),
                )
                .truncate(),
            );
        });
    };

    if cols == 4 {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            card_sessions(ui);
            card_tokens(ui);
            card_cache(ui);
            card_compactions(ui);
        });
    } else {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            card_sessions(ui);
            card_tokens(ui);
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            card_cache(ui);
            card_compactions(ui);
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
struct DailyPlotSeries {
    base_date: Option<NaiveDate>,
    input_pts: Vec<[f64; 2]>,
    cached_pts: Vec<[f64; 2]>,
    output_pts: Vec<[f64; 2]>,
    fallback_labels: Vec<String>,
}

fn build_daily_plot_series(daily: &[DailyUsagePoint]) -> DailyPlotSeries {
    let parsed_dates: Vec<Option<NaiveDate>> = daily
        .iter()
        .map(|d| NaiveDate::parse_from_str(&d.date_ymd, "%Y-%m-%d").ok())
        .collect();
    let base_date = parsed_dates.iter().flatten().copied().min();

    let mut input_pts = Vec::with_capacity(daily.len());
    let mut cached_pts = Vec::with_capacity(daily.len());
    let mut output_pts = Vec::with_capacity(daily.len());
    let mut fallback_labels = Vec::with_capacity(daily.len());

    for (idx, (d, parsed)) in daily.iter().zip(parsed_dates.iter()).enumerate() {
        let x = match (base_date, parsed) {
            (Some(base), Some(date)) => (*date - base).num_days() as f64,
            _ => idx as f64,
        };
        input_pts.push([x, d.input_tokens as f64]);
        cached_pts.push([x, d.cached_tokens as f64]);
        output_pts.push([x, d.output_tokens as f64]);
        fallback_labels.push(d.date_ymd.get(5..).unwrap_or(&d.date_ymd).to_string());
    }

    DailyPlotSeries {
        base_date,
        input_pts,
        cached_pts,
        output_pts,
        fallback_labels,
    }
}

fn format_plot_day(
    x: f64,
    base_date: Option<NaiveDate>,
    fallback_labels: &[String],
    date_fmt: &str,
) -> Option<String> {
    let rounded = x.round();
    if rounded < 0.0 || !rounded.is_finite() {
        return None;
    }
    let offset = rounded as u64;
    if let Some(base) = base_date {
        base.checked_add_days(Days::new(offset))
            .map(|date| date.format(date_fmt).to_string())
    } else {
        usize::try_from(offset)
            .ok()
            .and_then(|i| fallback_labels.get(i))
            .cloned()
    }
}

fn render_daily_plot(ui: &mut egui::Ui, daily: &[DailyUsagePoint], palette: &Palette) {
    ui.label(egui::RichText::new("日別トークン推移").strong());
    if daily.is_empty() {
        ui.label(
            egui::RichText::new("日別データはありません")
                .small()
                .color(palette.text_muted),
        );
        return;
    }

    let plot_w = ui.available_width().max(120.0);
    let plot_h = 165.0;

    let series = build_daily_plot_series(daily);
    let max_y = daily
        .iter()
        .map(|d| d.input_tokens.max(d.cached_tokens).max(d.output_tokens))
        .max()
        .unwrap_or(0) as f64;
    let x_axis_base = series.base_date;
    let x_axis_fallback = series.fallback_labels.clone();
    let label_base = series.base_date;
    let label_fallback = series.fallback_labels;
    let input_pts = series.input_pts;
    let cached_pts = series.cached_pts;
    let output_pts = series.output_pts;

    Plot::new("usage_daily_plot")
        .height(plot_h)
        .width(plot_w)
        .allow_scroll(false)
        .allow_zoom(false)
        .allow_drag(false)
        .include_y(0.0)
        .include_y((max_y * 1.1).max(10.0))
        .x_axis_formatter(move |mark, _range| {
            let rounded = mark.value.round();
            if (mark.value - rounded).abs() < 0.01 {
                return format_plot_day(rounded, x_axis_base, &x_axis_fallback, "%m-%d")
                    .unwrap_or_default();
            }
            String::new()
        })
        .label_formatter(move |pos| match pos {
            HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } => {
                let date_str = format_plot_day(position.x, label_base, &label_fallback, "%Y-%m-%d")
                    .unwrap_or_else(|| format!("{:.0}", position.x));
                let tok_str = format_tokens(position.y.round().max(0.0) as u64);
                if plot_name.is_empty() {
                    Some(format!("{date_str}: {tok_str}"))
                } else {
                    Some(format!("{plot_name}\n{date_str}: {tok_str}"))
                }
            }
            HoverPosition::Elsewhere { position } => {
                let date_str =
                    format_plot_day(position.x, label_base, &label_fallback, "%Y-%m-%d")?;
                let tok_str = format_tokens(position.y.round().max(0.0) as u64);
                Some(format!("{date_str}: {tok_str}"))
            }
        })
        .legend(Legend::default().position(Corner::LeftTop))
        .show(ui, |plot_ui| {
            plot_ui.line(
                Line::new("入力", PlotPoints::from(input_pts.clone()))
                    .color(palette.accent)
                    .width(2.0),
            );
            plot_ui.points(
                Points::new("", PlotPoints::from(input_pts))
                    .color(palette.accent)
                    .radius(3.0),
            );
            plot_ui.line(
                Line::new("キャッシュ", PlotPoints::from(cached_pts.clone()))
                    .color(palette.cache_line)
                    .width(2.0),
            );
            plot_ui.points(
                Points::new("", PlotPoints::from(cached_pts))
                    .color(palette.cache_line)
                    .radius(3.0),
            );
            plot_ui.line(
                Line::new("出力", PlotPoints::from(output_pts.clone()))
                    .color(palette.ok)
                    .width(2.0),
            );
            plot_ui.points(
                Points::new("", PlotPoints::from(output_pts))
                    .color(palette.ok)
                    .radius(3.0),
            );
        });
}

fn render_breakdown_table(
    ui: &mut egui::Ui,
    title: &str,
    key_col_header: &str,
    id_salt: &str,
    rows: &[UsageBreakdownRow],
    project_filter: &str,
    palette: &Palette,
) {
    let query = project_filter.trim().to_lowercase();
    let matched_count = if query.is_empty() {
        rows.len()
    } else {
        rows.iter()
            .filter(|r| row_matches_filter(r, &query))
            .count()
    };

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(title).strong());
        if !query.is_empty() {
            ui.label(
                egui::RichText::new(format!("(一致 {matched_count} / {} 件)", rows.len()))
                    .small()
                    .color(palette.accent),
            );
        }
    });

    if rows.is_empty() {
        ui.label(
            egui::RichText::new("データはありません")
                .small()
                .color(palette.text_muted),
        );
        return;
    }

    if !query.is_empty() && matched_count == 0 {
        ui.label(
            egui::RichText::new("一致する項目はありません")
                .small()
                .color(palette.text_muted),
        );
        return;
    }

    egui::ScrollArea::horizontal()
        .id_salt(format!("{id_salt}_hscroll"))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new(id_salt)
                .striped(true)
                .num_columns(6)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.strong(key_col_header);
                    ui.strong("セッション");
                    ui.strong("入力");
                    ui.strong("キャッシュ");
                    ui.strong("出力");
                    ui.strong("圧縮");
                    ui.end_row();

                    for row in rows {
                        let is_match = !query.is_empty() && row_matches_filter(row, &query);
                        if !query.is_empty() && !is_match {
                            continue;
                        }
                        let label_text = if row.label.is_empty() {
                            &row.key
                        } else {
                            &row.label
                        };
                        let label_color = if is_match {
                            palette.accent
                        } else {
                            palette.text_primary
                        };
                        let mut rich =
                            egui::RichText::new(truncate_ui(label_text, 40)).color(label_color);
                        if is_match {
                            rich = rich.strong();
                        }
                        let resp = ui.label(rich);
                        if !row.key.is_empty() && row.key != row.label {
                            resp.on_hover_text(&row.key);
                        }

                        ui.monospace(row.session_count.to_string());
                        ui.monospace(format_tokens(row.input_tokens));
                        ui.monospace(format_tokens(row.cached_tokens));
                        ui.monospace(format_tokens(row.output_tokens));
                        ui.monospace(row.compaction_count.to_string());
                        ui.end_row();
                    }
                });
        });
}

fn render_customizations_view(
    ui: &mut egui::Ui,
    state: &mut UsageViewState,
    customizations: Option<&CustomizationInventory>,
    customizations_loading: bool,
    palette: &Palette,
    actions: &mut UsageViewActions,
) {
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!customizations_loading, egui::Button::new("再スキャン"))
            .clicked()
        {
            actions.refresh_customizations = true;
        }

        if customizations_loading {
            ui.spinner();
            ui.label(
                egui::RichText::new("スキャン中...")
                    .small()
                    .color(palette.text_muted),
            );
        }

        if let Some(inv) = customizations {
            ui.separator();
            ui.label(
                egui::RichText::new(format!("MCPサーバー: {}", inv.mcp_server_count))
                    .color(palette.accent)
                    .strong(),
            );
            ui.label(
                egui::RichText::new(format!("プラグイン: {}", inv.plugin_count))
                    .color(palette.text_primary)
                    .strong(),
            );
            ui.label(
                egui::RichText::new(format!("総項目: {} 件", inv.items.len()))
                    .small()
                    .color(palette.text_muted),
            );
        }
    });

    let Some(inv) = customizations else {
        ui.add_space(12.0);
        let msg = if customizations_loading {
            "カスタマイズ設定をスキャンしています..."
        } else {
            "棚卸しデータがありません（「再スキャン」で取得できます）"
        };
        ui.label(egui::RichText::new(msg).color(palette.text_muted));
        return;
    };

    ui.add_space(8.0);
    render_customization_breakdown_summary(ui, &inv.breakdown, palette);

    ui.add_space(8.0);
    render_customization_filters(ui, state, palette);

    let filtered: Vec<&CustomizationItem> = inv
        .items
        .iter()
        .filter(|item| item_matches_filters(item, state))
        .collect();

    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("検出項目一覧").strong());
        ui.label(
            egui::RichText::new(format!("({} / {} 件)", filtered.len(), inv.items.len()))
                .small()
                .color(palette.text_muted),
        );
    });

    if filtered.is_empty() {
        ui.label(
            egui::RichText::new("条件に一致する項目はありません")
                .small()
                .color(palette.text_muted),
        );
    } else {
        egui::ScrollArea::horizontal()
            .id_salt("custom_items_hscroll")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("custom_items_table")
                    .striped(true)
                    .num_columns(5)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.strong("名前");
                        ui.strong("種別");
                        ui.strong("タイミング");
                        ui.strong("ソース");
                        ui.strong("推定トークン");
                        ui.end_row();

                        for item in &filtered {
                            let path_text = item.path.display().to_string();
                            ui.label(truncate_ui(&item.name, 36))
                                .on_hover_text(&path_text);

                            let kind_color = match item.kind {
                                CustomizationKind::McpTool => palette.accent,
                                CustomizationKind::McpInstruction => palette.cache_line,
                                CustomizationKind::RuleFile => palette.ok,
                                CustomizationKind::SkillIndex => palette.bucket_thinking,
                                CustomizationKind::CustomAgent => palette.warn,
                            };
                            ui.label(egui::RichText::new(kind_label(item.kind)).color(kind_color));

                            let timing_color = match item.timing {
                                LoadTiming::Always => palette.warn,
                                LoadTiming::Deferred => palette.ok,
                            };
                            ui.label(
                                egui::RichText::new(timing_label(item.timing))
                                    .color(timing_color)
                                    .strong(),
                            );

                            ui.label(
                                egui::RichText::new(truncate_ui(&item.source, 28))
                                    .color(palette.text_muted),
                            )
                            .on_hover_text(&path_text);

                            ui.monospace(format_tokens(item.estimated_tokens));
                            ui.end_row();
                        }
                    });
            });
    }

    if !inv.scan_warnings.is_empty() {
        ui.add_space(8.0);
        egui::CollapsingHeader::new(
            egui::RichText::new(format!("スキャン警告 ({} 件)", inv.scan_warnings.len()))
                .color(palette.warn),
        )
        .default_open(false)
        .show(ui, |ui| {
            for warning in &inv.scan_warnings {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(warning)
                            .small()
                            .color(palette.text_muted),
                    )
                    .wrap(),
                );
            }
        });
    }
}

fn render_customization_breakdown_summary(
    ui: &mut egui::Ui,
    breakdown: &CustomizationBreakdown,
    palette: &Palette,
) {
    let always_total = breakdown.mcp_tools
        + breakdown.mcp_instructions
        + breakdown.rules_files
        + breakdown.skills_index
        + breakdown.custom_agents;
    let deferred_total =
        breakdown.mcp_deferred + breakdown.skills_deferred + breakdown.rules_deferred;

    let avail = ui.available_width();
    let spacing = 8.0;
    let side_by_side = avail >= 520.0;
    let card_w = if side_by_side {
        ((avail - spacing) / 2.0).max(120.0)
    } else {
        avail.max(120.0)
    };

    let render_always = |ui: &mut egui::Ui| {
        kpi_card(ui, card_w, palette, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("常時読込 (Always)")
                        .strong()
                        .color(palette.warn),
                );
                ui.monospace(
                    egui::RichText::new(format!("{} tok", format_tokens(always_total)))
                        .strong()
                        .color(palette.text_primary),
                );
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                breakdown_chip(ui, "MCPツール", breakdown.mcp_tools, palette);
                breakdown_chip(ui, "MCP指示", breakdown.mcp_instructions, palette);
                breakdown_chip(ui, "ルール", breakdown.rules_files, palette);
                breakdown_chip(ui, "スキル索引", breakdown.skills_index, palette);
                breakdown_chip(ui, "エージェント", breakdown.custom_agents, palette);
            });
        });
    };

    let render_deferred = |ui: &mut egui::Ui| {
        kpi_card(ui, card_w, palette, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("遅延読込 (Deferred)")
                        .strong()
                        .color(palette.ok),
                );
                ui.monospace(
                    egui::RichText::new(format!("{} tok", format_tokens(deferred_total)))
                        .strong()
                        .color(palette.text_primary),
                );
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                breakdown_chip(ui, "MCP遅延", breakdown.mcp_deferred, palette);
                breakdown_chip(ui, "スキル本体", breakdown.skills_deferred, palette);
                breakdown_chip(ui, "条件ルール", breakdown.rules_deferred, palette);
            });
        });
    };

    if side_by_side {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            render_always(ui);
            render_deferred(ui);
        });
    } else {
        render_always(ui);
        ui.add_space(6.0);
        render_deferred(ui);
    }
}

fn breakdown_chip(ui: &mut egui::Ui, label: &str, tokens: u64, palette: &Palette) {
    ui.label(
        egui::RichText::new(format!("{label}: {}", format_tokens(tokens)))
            .small()
            .color(palette.text_muted),
    );
}

fn render_customization_filters(ui: &mut egui::Ui, state: &mut UsageViewState, palette: &Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("検索:").color(palette.text_muted));
        let search_w = (ui.available_width() - 80.0).clamp(90.0, 220.0);
        ui.add(
            egui::TextEdit::singleline(&mut state.customization_search)
                .id_salt("custom_search_input")
                .hint_text("名前・ソースで絞込")
                .desired_width(search_w),
        );

        let has_any_filter = !state.customization_search.is_empty()
            || state.timing_filter.is_some()
            || state.kind_filter.is_some();
        if has_any_filter && ui.small_button("フィルタ解除").clicked() {
            state.customization_search.clear();
            state.timing_filter = None;
            state.kind_filter = None;
        }
    });

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("読込:")
                .small()
                .color(palette.text_muted),
        );
        ui.selectable_value(&mut state.timing_filter, None, "すべて");
        ui.selectable_value(&mut state.timing_filter, Some(LoadTiming::Always), "常時");
        ui.selectable_value(&mut state.timing_filter, Some(LoadTiming::Deferred), "遅延");
    });

    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("種別:")
                .small()
                .color(palette.text_muted),
        );
        ui.selectable_value(&mut state.kind_filter, None, "すべて");
        for kind in ALL_KINDS {
            ui.selectable_value(&mut state.kind_filter, Some(kind), kind_label(kind));
        }
    });
}

fn kpi_card(
    ui: &mut egui::Ui,
    width: f32,
    palette: &Palette,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let margin = 8_i8;
    let chrome = f32::from(margin) * 2.0 + 2.0;
    egui::Frame::group(ui.style())
        .fill(palette.bg_card)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .inner_margin(egui::Margin::same(margin))
        .show(ui, |ui| {
            ui.set_width((width - chrome).max(60.0));
            ui.vertical(add_contents);
        });
}

fn row_matches_filter(row: &UsageBreakdownRow, query_lower: &str) -> bool {
    row.key.to_lowercase().contains(query_lower) || row.label.to_lowercase().contains(query_lower)
}

fn item_matches_filters(item: &CustomizationItem, state: &UsageViewState) -> bool {
    if let Some(timing) = state.timing_filter
        && item.timing != timing
    {
        return false;
    }
    if let Some(kind) = state.kind_filter
        && item.kind != kind
    {
        return false;
    }
    let query = state.customization_search.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    item.name.to_lowercase().contains(&query)
        || item.source.to_lowercase().contains(&query)
        || item.path.to_string_lossy().to_lowercase().contains(&query)
        || kind_label(item.kind).to_lowercase().contains(&query)
        || timing_label(item.timing).to_lowercase().contains(&query)
}

fn period_label(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Today => "今日",
        UsagePeriod::Last7Days => "7日間",
        UsagePeriod::Last30Days => "30日間",
        UsagePeriod::AllTime => "全期間",
    }
}

fn kind_label(kind: CustomizationKind) -> &'static str {
    match kind {
        CustomizationKind::McpTool => "MCPツール",
        CustomizationKind::McpInstruction => "MCP指示",
        CustomizationKind::RuleFile => "ルール",
        CustomizationKind::SkillIndex => "スキル索引",
        CustomizationKind::CustomAgent => "エージェント",
    }
}

fn timing_label(timing: LoadTiming) -> &'static str {
    match timing {
        LoadTiming::Always => "常時",
        LoadTiming::Deferred => "遅延",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::fonts::install_japanese_fonts;
    use crate::theme::{ThemeMode, apply_theme};

    fn sample_usage_report(truncated: bool) -> UsageReport {
        UsageReport {
            period: UsagePeriod::Last7Days,
            total_sessions: if truncated { 620 } else { 18 },
            analyzed_sessions: if truncated { 500 } else { 18 },
            skipped_sessions: 1,
            total_input_tokens: 1_250_000,
            total_cached_tokens: 900_000,
            total_output_tokens: 85_000,
            cache_hit_ratio: 0.72,
            total_compactions: 4,
            daily: vec![
                DailyUsagePoint {
                    date_ymd: "2026-11-22".to_string(),
                    session_count: 5,
                    input_tokens: 300_000,
                    cached_tokens: 210_000,
                    output_tokens: 20_000,
                    compaction_count: 1,
                },
                DailyUsagePoint {
                    date_ymd: "2026-11-23".to_string(),
                    session_count: 7,
                    input_tokens: 550_000,
                    cached_tokens: 400_000,
                    output_tokens: 38_000,
                    compaction_count: 2,
                },
                DailyUsagePoint {
                    date_ymd: "2026-11-24".to_string(),
                    session_count: 6,
                    input_tokens: 400_000,
                    cached_tokens: 290_000,
                    output_tokens: 27_000,
                    compaction_count: 1,
                },
            ],
            by_model: vec![UsageBreakdownRow {
                key: "gemini-2.5-pro".to_string(),
                label: "Gemini 2.5 Pro".to_string(),
                session_count: 18,
                input_tokens: 1_250_000,
                cached_tokens: 900_000,
                output_tokens: 85_000,
                compaction_count: 4,
            }],
            by_project: vec![
                UsageBreakdownRow {
                    key: "C:/repo/agy-monitor".to_string(),
                    label: "agy-monitor".to_string(),
                    session_count: 12,
                    input_tokens: 950_000,
                    cached_tokens: 720_000,
                    output_tokens: 65_000,
                    compaction_count: 3,
                },
                UsageBreakdownRow {
                    key: "C:/repo/other-app".to_string(),
                    label: "other-app".to_string(),
                    session_count: 6,
                    input_tokens: 300_000,
                    cached_tokens: 180_000,
                    output_tokens: 20_000,
                    compaction_count: 1,
                },
            ],
            by_surface: vec![UsageBreakdownRow {
                key: "app".to_string(),
                label: "app".to_string(),
                session_count: 18,
                input_tokens: 1_250_000,
                cached_tokens: 900_000,
                output_tokens: 85_000,
                compaction_count: 4,
            }],
            truncated,
        }
    }

    fn sample_inventory(item_count: usize) -> CustomizationInventory {
        let kinds = [
            CustomizationKind::McpTool,
            CustomizationKind::McpInstruction,
            CustomizationKind::RuleFile,
            CustomizationKind::SkillIndex,
            CustomizationKind::CustomAgent,
        ];
        let items = (0..item_count)
            .map(|i| {
                let kind = kinds[i % kinds.len()];
                let timing = if i % 2 == 0 {
                    LoadTiming::Always
                } else {
                    LoadTiming::Deferred
                };
                CustomizationItem {
                    kind,
                    timing,
                    name: format!("item-{i:02}"),
                    source: if i % 3 == 0 {
                        "alpha-plugin".to_string()
                    } else {
                        "builtin-mcp".to_string()
                    },
                    path: PathBuf::from(format!("C:/users/dev/.gemini/item-{i:02}.md")),
                    estimated_tokens: 120 + (i as u64) * 35,
                }
            })
            .collect();

        CustomizationInventory {
            items,
            breakdown: CustomizationBreakdown {
                mcp_tools: 4_200,
                mcp_instructions: 1_100,
                rules_files: 900,
                skills_index: 640,
                custom_agents: 480,
                mcp_deferred: 2_300,
                skills_deferred: 5_400,
                rules_deferred: 750,
            },
            mcp_server_count: 3,
            plugin_count: 2,
            scan_warnings: vec!["oversized file skipped: example.json".to_string()],
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

    fn click_events(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn assert_no_cost_currency_text(texts: &[(String, egui::Rect, egui::Rect)]) {
        for (txt, _, _) in texts {
            let lower = txt.to_lowercase();
            for forbidden in ["$", "円", "usd", "jpy", "cost", "料金", "課金"] {
                assert!(
                    !lower.contains(forbidden),
                    "forbidden cost/currency token '{forbidden}' found in rendered text: {txt:?}"
                );
            }
        }
    }

    fn assert_clips_within_viewport(
        shapes: &[egui::epaint::ClippedShape],
        width: f32,
        height: f32,
        context: &str,
    ) {
        for s in shapes {
            assert!(
                s.clip_rect.left() >= -0.5
                    && s.clip_rect.top() >= -0.5
                    && s.clip_rect.right() <= width + 0.5
                    && s.clip_rect.bottom() <= height + 0.5,
                "[{context}] clip_rect {:?} exceeded viewport {width}x{height}",
                s.clip_rect
            );
        }
    }

    #[test]
    fn gui_11_usage_phase2_narrow_viewport_renders_within_bounds() {
        let report = sample_usage_report(true);
        let inventory = sample_inventory(6);
        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);

        for (w, h) in [(520.0_f32, 480.0_f32), (760.0_f32, 520.0_f32)] {
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                apply_theme(&ctx, mode);
                let palette = Palette::for_mode(mode);

                for sub_tab in [Phase2SubTab::UsageStats, Phase2SubTab::Customizations] {
                    let mut state = UsageViewState {
                        sub_tab,
                        project_filter: "agy-monitor".to_string(),
                        ..Default::default()
                    };

                    for frame_idx in 0..2 {
                        let raw_input = egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(w, h),
                            )),
                            ..Default::default()
                        };
                        let mut actions = UsageViewActions::default();
                        let mut output = ctx.run_ui(raw_input, |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                actions = render_usage_phase2(
                                    ui,
                                    &mut state,
                                    Some(&report),
                                    false,
                                    Some(&inventory),
                                    false,
                                    &palette,
                                );
                            });
                        });
                        assert_clips_within_viewport(
                            &output.shapes,
                            w,
                            h,
                            &format!("{w}x{h} {mode:?} {sub_tab:?} frame={frame_idx}"),
                        );
                        let texts = collect_rendered_texts(&output.shapes);
                        assert_no_cost_currency_text(&texts);
                        assert!(actions.refresh_usage.is_none());
                        assert!(!actions.refresh_customizations);
                        output.textures_delta.clear();
                    }
                }
            }
        }

        // Verify period switch emits actions.refresh_usage and updates state.selected_period.
        apply_theme(&ctx, ThemeMode::Dark);
        let palette = Palette::for_mode(ThemeMode::Dark);
        let mut state = UsageViewState::default();
        let viewport = egui::vec2(520.0, 480.0);

        let run_frame = |state: &mut UsageViewState, events: Vec<egui::Event>| {
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                events,
                ..Default::default()
            };
            let mut actions = UsageViewActions::default();
            let mut output = ctx.run_ui(raw_input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    actions = render_usage_phase2(
                        ui,
                        state,
                        Some(&report),
                        false,
                        Some(&inventory),
                        false,
                        &palette,
                    );
                });
            });
            assert_clips_within_viewport(&output.shapes, viewport.x, viewport.y, "period_switch");
            let texts = collect_rendered_texts(&output.shapes);
            assert_no_cost_currency_text(&texts);
            output.textures_delta.clear();
            (texts, actions)
        };

        let _ = run_frame(&mut state, vec![]);
        let (texts, _) = run_frame(&mut state, vec![]);
        assert!(
            texts
                .iter()
                .any(|(t, _, _)| t.contains("上限を超えたため直近 500 / 620 件")),
            "expected truncated notice in texts: {:?}",
            texts.iter().map(|(t, _, _)| t).collect::<Vec<_>>()
        );

        let btn_30d_pos = texts
            .iter()
            .find(|(t, _, _)| t == "30日間")
            .expect("30日間 period button")
            .1
            .center();
        let (_, actions_30d) = run_frame(&mut state, click_events(btn_30d_pos));
        assert_eq!(state.selected_period, UsagePeriod::Last30Days);
        assert_eq!(actions_30d.refresh_usage, Some(UsagePeriod::Last30Days));

        let custom_tab_pos = texts
            .iter()
            .find(|(t, _, _)| t == "カスタマイズ / MCP 棚卸し")
            .expect("customizations sub-tab button")
            .1
            .center();
        let (texts_custom, _) = run_frame(&mut state, click_events(custom_tab_pos));
        assert_eq!(state.sub_tab, Phase2SubTab::Customizations);

        let rescan_pos = texts_custom
            .iter()
            .find(|(t, _, _)| t == "再スキャン")
            .expect("再スキャン button")
            .1
            .center();
        let (_, actions_rescan) = run_frame(&mut state, click_events(rescan_pos));
        assert!(actions_rescan.refresh_customizations);
    }

    #[test]
    fn gui_12_customization_search_filter_and_scroll() {
        let mut inventory = sample_inventory(40);
        // Add one distinct item matching search + Deferred + RuleFile so filter precision is verified.
        inventory.items.push(CustomizationItem {
            kind: CustomizationKind::RuleFile,
            timing: LoadTiming::Deferred,
            name: "omega-deferred-rule".to_string(),
            source: "custom-plugin".to_string(),
            path: PathBuf::from("C:/users/dev/.gemini/rules/omega.md"),
            estimated_tokens: 450,
        });
        inventory.items.push(CustomizationItem {
            kind: CustomizationKind::RuleFile,
            timing: LoadTiming::Always,
            name: "omega-always-rule".to_string(),
            source: "custom-plugin".to_string(),
            path: PathBuf::from("C:/users/dev/.gemini/rules/always.md"),
            estimated_tokens: 300,
        });

        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        apply_theme(&ctx, ThemeMode::Dark);
        let palette = Palette::for_mode(ThemeMode::Dark);
        let viewport = egui::vec2(520.0, 420.0);

        let mut state = UsageViewState {
            sub_tab: Phase2SubTab::Customizations,
            ..Default::default()
        };

        let mut frame_time = 0.0_f64;
        let mut run_frame = |state: &mut UsageViewState, events: Vec<egui::Event>| {
            frame_time += 0.05;
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                time: Some(frame_time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw_input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ = render_usage_phase2(
                        ui,
                        state,
                        None,
                        false,
                        Some(&inventory),
                        false,
                        &palette,
                    );
                });
            });
            assert_clips_within_viewport(&output.shapes, viewport.x, viewport.y, "gui_12");
            let texts = collect_rendered_texts(&output.shapes);
            assert_no_cost_currency_text(&texts);
            output.textures_delta.clear();
            texts
        };

        // Initial settle frames with 42 items in 520x420 viewport
        let _ = run_frame(&mut state, vec![]);
        let texts_top = run_frame(&mut state, vec![]);
        assert!(
            texts_top.iter().any(|(t, _, _)| t == "(42 / 42 件)"),
            "expected unfiltered count (42 / 42 件)"
        );

        // Scroll down with mouse wheel so bottom items come into view cleanly within viewport bounds
        let scroll_center = egui::pos2(260.0, 260.0);
        let mut reached_item_39 = None;
        for _ in 0..12 {
            let texts = run_frame(
                &mut state,
                vec![
                    egui::Event::PointerMoved(scroll_center),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -300.0),
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Unknown,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            if let Some((_, rect, _)) = texts.iter().find(|(t, _, _)| t == "item-39")
                && rect.top() >= 0.0
                && rect.bottom() <= viewport.y + 0.5
            {
                reached_item_39 = Some(*rect);
                break;
            }
        }
        let item_39_rect =
            reached_item_39.expect("item-39 should be reachable after scrolling in 520x420");
        assert!(
            item_39_rect.top() >= 0.0 && item_39_rect.bottom() <= viewport.y + 0.5,
            "scrolled item-39 rect {:?} should be inside visible viewport 0..{}",
            item_39_rect,
            viewport.y
        );

        // Apply search query + timing filter + kind filter
        state.customization_search = "omega".to_string();
        state.timing_filter = Some(LoadTiming::Deferred);
        state.kind_filter = Some(CustomizationKind::RuleFile);

        let _ = run_frame(&mut state, vec![]);
        let texts_filtered = run_frame(&mut state, vec![]);
        assert!(
            texts_filtered.iter().any(|(t, _, _)| t == "(1 / 42 件)"),
            "expected filtered count (1 / 42 件); got {:?}",
            texts_filtered
                .iter()
                .map(|(t, _, _)| t)
                .filter(|t| t.contains("件"))
                .collect::<Vec<_>>()
        );
        assert!(
            texts_filtered
                .iter()
                .any(|(t, _, _)| t == "omega-deferred-rule"),
            "omega-deferred-rule must appear"
        );
        assert!(
            !texts_filtered
                .iter()
                .any(|(t, _, _)| t == "omega-always-rule"),
            "omega-always-rule must be excluded by timing_filter"
        );
        assert!(
            !texts_filtered.iter().any(|(t, _, _)| t == "item-00"),
            "non-matching items must be excluded"
        );
    }

    #[test]
    fn gui_13_project_filter_hides_non_matching_rows_and_daily_plot_uses_date_spacing() {
        // 1. Verify sparse dates map to calendar day offsets rather than sequential indices.
        let sparse_daily = vec![
            DailyUsagePoint {
                date_ymd: "2026-11-01".to_string(),
                session_count: 2,
                input_tokens: 100_000,
                cached_tokens: 50_000,
                output_tokens: 10_000,
                compaction_count: 0,
            },
            DailyUsagePoint {
                date_ymd: "2026-11-02".to_string(),
                session_count: 3,
                input_tokens: 200_000,
                cached_tokens: 100_000,
                output_tokens: 20_000,
                compaction_count: 1,
            },
            DailyUsagePoint {
                date_ymd: "2026-11-24".to_string(),
                session_count: 4,
                input_tokens: 300_000,
                cached_tokens: 150_000,
                output_tokens: 30_000,
                compaction_count: 2,
            },
        ];
        let series = build_daily_plot_series(&sparse_daily);
        assert_eq!(series.base_date, NaiveDate::from_ymd_opt(2026, 11, 1));
        let xs = |pts: &[[f64; 2]]| pts.iter().map(|p| p[0]).collect::<Vec<_>>();
        assert_eq!(xs(&series.input_pts), vec![0.0, 1.0, 23.0]);
        assert_eq!(xs(&series.cached_pts), vec![0.0, 1.0, 23.0]);
        assert_eq!(xs(&series.output_pts), vec![0.0, 1.0, 23.0]);
        assert_eq!(
            format_plot_day(0.0, series.base_date, &series.fallback_labels, "%m-%d"),
            Some("11-01".to_string())
        );
        assert_eq!(
            format_plot_day(23.0, series.base_date, &series.fallback_labels, "%Y-%m-%d"),
            Some("2026-11-24".to_string())
        );

        // Fallback when date strings cannot be parsed.
        let invalid_daily = vec![
            DailyUsagePoint {
                date_ymd: "invalid-a".to_string(),
                ..sparse_daily[0].clone()
            },
            DailyUsagePoint {
                date_ymd: "invalid-b".to_string(),
                ..sparse_daily[1].clone()
            },
        ];
        let fallback_series = build_daily_plot_series(&invalid_daily);
        assert_eq!(fallback_series.base_date, None);
        assert_eq!(xs(&fallback_series.input_pts), vec![0.0, 1.0]);

        // 2. Verify project filter hides non-matching rows in narrow viewport UI while keeping
        //    model/surface tables intact and displaying the aggregation-basis note.
        let mut report = sample_usage_report(false);
        report.daily = sparse_daily;

        let ctx = egui::Context::default();
        install_japanese_fonts(&ctx);
        apply_theme(&ctx, ThemeMode::Dark);
        let palette = Palette::for_mode(ThemeMode::Dark);

        let run_stats_frame = |state: &mut UsageViewState, viewport: egui::Vec2, label: &str| {
            let raw_input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw_input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ =
                        render_usage_phase2(ui, state, Some(&report), false, None, false, &palette);
                });
            });
            assert_clips_within_viewport(&output.shapes, viewport.x, viewport.y, label);
            let texts = collect_rendered_texts(&output.shapes);
            assert_no_cost_currency_text(&texts);
            output.textures_delta.clear();
            texts
        };

        let mut state = UsageViewState {
            sub_tab: Phase2SubTab::UsageStats,
            project_filter: "agy-monitor".to_string(),
            ..Default::default()
        };

        // Short narrow viewport (520x480) checks clipping and top-of-view aggregation note.
        let short_narrow = egui::vec2(520.0, 480.0);
        let _ = run_stats_frame(&mut state, short_narrow, "gui_13_short_warmup");
        let texts_short = run_stats_frame(&mut state, short_narrow, "gui_13_short");
        assert!(
            texts_short.iter().any(|(t, _, _)| t
                .contains("※集計基準: 会話の最終更新日（ローカル日付）ごとにセッション累計を集計")),
            "aggregation basis note must be rendered"
        );

        // Tall narrow viewport (520x1100) keeps all three breakdown tables visible at once.
        let tall_narrow = egui::vec2(520.0, 1100.0);
        let _ = run_stats_frame(&mut state, tall_narrow, "gui_13_match_warmup");
        let texts_match = run_stats_frame(&mut state, tall_narrow, "gui_13_match");

        assert!(
            texts_match.iter().any(|(t, _, _)| t == "(一致 1 / 2 件)"),
            "expected matched count indicator (一致 1 / 2 件)"
        );
        assert!(
            texts_match.iter().any(|(t, _, _)| t == "950.0k"),
            "matching project row (agy-monitor / 950.0k) must be visible"
        );
        assert!(
            !texts_match
                .iter()
                .any(|(t, _, _)| t == "other-app" || t == "300.0k"),
            "non-matching project row (other-app / 300.0k) must be hidden"
        );
        assert!(
            texts_match.iter().any(|(t, _, _)| t == "Gemini 2.5 Pro"),
            "unfiltered model breakdown row must remain visible"
        );
        assert!(
            texts_match.iter().any(|(t, _, _)| t == "app"),
            "unfiltered surface breakdown row must remain visible"
        );

        // Zero-match filter should show empty-match notice without hiding model/surface tables.
        state.project_filter = "no-such-project".to_string();
        let _ = run_stats_frame(&mut state, tall_narrow, "gui_13_empty_warmup");
        let texts_empty = run_stats_frame(&mut state, tall_narrow, "gui_13_empty");

        assert!(
            texts_empty.iter().any(|(t, _, _)| t == "(一致 0 / 2 件)"),
            "expected zero-match count indicator (一致 0 / 2 件)"
        );
        assert!(
            texts_empty
                .iter()
                .any(|(t, _, _)| t == "一致する項目はありません"),
            "expected empty-match notice"
        );
        assert!(
            !texts_empty
                .iter()
                .any(|(t, _, _)| t == "agy-monitor" || t == "other-app" || t == "950.0k"),
            "all project rows must be hidden when no project matches"
        );
        assert!(
            texts_empty.iter().any(|(t, _, _)| t == "Gemini 2.5 Pro"),
            "unfiltered model breakdown row must remain visible even when project match is 0"
        );
        assert!(
            texts_empty.iter().any(|(t, _, _)| t == "app"),
            "unfiltered surface breakdown row must remain visible even when project match is 0"
        );
    }
}
