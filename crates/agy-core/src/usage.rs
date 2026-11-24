use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Days, Local, NaiveDate};

use crate::analyzer::analyze_conversation_with_meta;
use crate::fingerprint::{DbFingerprint, TranscriptFingerprint};
use crate::models::normalize_model_slug;
use crate::sessions::{SessionIndex, SessionSummary, is_valid_conv_id, parse_datetime_ms};

pub const DEFAULT_MAX_USAGE_SESSIONS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UsagePeriod {
    Today,
    #[default]
    Last7Days,
    Last30Days,
    AllTime,
}

impl UsagePeriod {
    pub fn label(self) -> &'static str {
        match self {
            UsagePeriod::Today => "今日",
            UsagePeriod::Last7Days => "7日間",
            UsagePeriod::Last30Days => "30日間",
            UsagePeriod::AllTime => "全期間",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailyUsagePoint {
    pub date_ymd: String,
    pub session_count: usize,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub compaction_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageBreakdownRow {
    pub key: String,
    pub label: String,
    pub session_count: usize,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub compaction_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageReport {
    pub period: UsagePeriod,
    pub total_sessions: usize,
    pub analyzed_sessions: usize,
    pub skipped_sessions: usize,
    pub total_input_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_output_tokens: u64,
    pub cache_hit_ratio: f64,
    pub total_compactions: usize,
    pub daily: Vec<DailyUsagePoint>,
    pub by_model: Vec<UsageBreakdownRow>,
    pub by_project: Vec<UsageBreakdownRow>,
    pub by_surface: Vec<UsageBreakdownRow>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
struct SessionUsageMetrics {
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    compaction_count: usize,
    model_key: String,
    model_label: String,
    last_step_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct CachedSessionUsage {
    transcript_fp: TranscriptFingerprint,
    db_fp: DbFingerprint,
    metrics: SessionUsageMetrics,
}

#[derive(Debug, Default)]
pub struct UsageCache {
    entries: HashMap<String, CachedSessionUsage>,
}

impl UsageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

pub fn aggregate_usage(
    index: &SessionIndex,
    period: UsagePeriod,
    max_sessions: usize,
    cache: &mut UsageCache,
    cancel: &AtomicBool,
) -> Option<UsageReport> {
    aggregate_usage_at(index, period, max_sessions, cache, cancel, Local::now())
}

pub fn aggregate_usage_at(
    index: &SessionIndex,
    period: UsagePeriod,
    max_sessions: usize,
    cache: &mut UsageCache,
    cancel: &AtomicBool,
    now_local: DateTime<Local>,
) -> Option<UsageReport> {
    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let today = now_local.date_naive();
    let mut candidates: Vec<(&SessionSummary, Option<NaiveDate>)> = index
        .by_id
        .values()
        .filter_map(|summary| {
            let session_date = local_date_from_ms(summary.effective_mtime_ms);
            if session_in_period(session_date, period, today) {
                Some((summary, session_date))
            } else {
                None
            }
        })
        .collect();

    candidates.sort_by(|(a, _), (b, _)| {
        b.effective_mtime_ms
            .cmp(&a.effective_mtime_ms)
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
    });

    let total_sessions = candidates.len();
    let truncated = total_sessions > max_sessions;

    let mut analyzed_sessions: usize = 0;
    let mut skipped_sessions: usize = 0;
    let mut total_input_tokens: u64 = 0;
    let mut total_cached_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_compactions: usize = 0;

    let mut daily_map: HashMap<String, DailyUsagePoint> = HashMap::new();
    let mut model_map: HashMap<String, UsageBreakdownRow> = HashMap::new();
    let mut project_map: HashMap<String, UsageBreakdownRow> = HashMap::new();
    let mut surface_map: HashMap<String, UsageBreakdownRow> = HashMap::new();

    for (summary, session_date) in candidates.into_iter().take(max_sessions) {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }

        let Some(metrics) = load_or_analyze_session(summary, cache) else {
            skipped_sessions += 1;
            continue;
        };

        analyzed_sessions += 1;
        total_input_tokens = total_input_tokens.saturating_add(metrics.input_tokens);
        total_cached_tokens = total_cached_tokens.saturating_add(metrics.cached_tokens);
        total_output_tokens = total_output_tokens.saturating_add(metrics.output_tokens);
        total_compactions = total_compactions.saturating_add(metrics.compaction_count);

        let effective_date =
            session_date.or_else(|| metrics.last_step_ms.and_then(local_date_from_ms));
        if let Some(date) = effective_date {
            let date_ymd = date.format("%Y-%m-%d").to_string();
            let day = daily_map
                .entry(date_ymd.clone())
                .or_insert_with(|| DailyUsagePoint {
                    date_ymd,
                    ..Default::default()
                });
            day.session_count += 1;
            day.input_tokens = day.input_tokens.saturating_add(metrics.input_tokens);
            day.cached_tokens = day.cached_tokens.saturating_add(metrics.cached_tokens);
            day.output_tokens = day.output_tokens.saturating_add(metrics.output_tokens);
            day.compaction_count = day
                .compaction_count
                .saturating_add(metrics.compaction_count);
        }

        add_breakdown(
            &mut model_map,
            &metrics.model_key,
            &metrics.model_label,
            &metrics,
        );

        let (project_key, project_label) = resolve_project_display(summary);
        add_breakdown(&mut project_map, &project_key, &project_label, &metrics);

        let surface_label = summary.surface.label();
        add_breakdown(&mut surface_map, surface_label, surface_label, &metrics);
    }

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let cache_hit_ratio = if total_input_tokens > 0 {
        (total_cached_tokens as f64 / total_input_tokens as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let mut daily: Vec<DailyUsagePoint> = daily_map.into_values().collect();
    daily.sort_by(|a, b| a.date_ymd.cmp(&b.date_ymd));

    let mut by_model: Vec<UsageBreakdownRow> = model_map.into_values().collect();
    sort_breakdown(&mut by_model);

    let mut by_project: Vec<UsageBreakdownRow> = project_map.into_values().collect();
    sort_breakdown(&mut by_project);

    let mut by_surface: Vec<UsageBreakdownRow> = surface_map.into_values().collect();
    sort_breakdown(&mut by_surface);

    Some(UsageReport {
        period,
        total_sessions,
        analyzed_sessions,
        skipped_sessions,
        total_input_tokens,
        total_cached_tokens,
        total_output_tokens,
        cache_hit_ratio,
        total_compactions,
        daily,
        by_model,
        by_project,
        by_surface,
        truncated,
    })
}

fn local_date_from_ms(ms: i64) -> Option<NaiveDate> {
    if ms <= 0 {
        return None;
    }
    DateTime::from_timestamp_millis(ms).map(|utc| utc.with_timezone(&Local).date_naive())
}

fn session_in_period(
    session_date: Option<NaiveDate>,
    period: UsagePeriod,
    today: NaiveDate,
) -> bool {
    match period {
        UsagePeriod::AllTime => true,
        UsagePeriod::Today => session_date == Some(today),
        UsagePeriod::Last7Days => {
            let Some(date) = session_date else {
                return false;
            };
            let start = today.checked_sub_days(Days::new(6)).unwrap_or(today);
            date >= start && date <= today
        }
        UsagePeriod::Last30Days => {
            let Some(date) = session_date else {
                return false;
            };
            let start = today.checked_sub_days(Days::new(29)).unwrap_or(today);
            date >= start && date <= today
        }
    }
}

fn load_or_analyze_session(
    summary: &SessionSummary,
    cache: &mut UsageCache,
) -> Option<SessionUsageMetrics> {
    let conv_id = &summary.conversation_id;
    if !is_valid_conv_id(conv_id) {
        return None;
    }

    let logs_dir = summary
        .app_data_dir
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    let tfp = TranscriptFingerprint::from_logs_dir(&logs_dir);
    if !tfp.exists() {
        return None;
    }

    let conv_db = summary
        .app_data_dir
        .join("conversations")
        .join(format!("{conv_id}.db"));
    let dfp = DbFingerprint::from_db_path(&conv_db);

    if let Some(cached) = cache.entries.get(conv_id)
        && cached.transcript_fp == tfp
        && cached.db_fp == dfp
    {
        return Some(cached.metrics.clone());
    }

    let (analysis, model_from_db) =
        analyze_conversation_with_meta(&summary.app_data_dir, conv_id, None, None).ok()?;
    if !analysis.found {
        return None;
    }

    let (model_key, model_label) = resolve_model_display(
        model_from_db,
        &analysis.model.model_id,
        &analysis.model.model_label,
        &summary.model_label,
    );
    let last_step_ms = analysis
        .last_step_at
        .as_deref()
        .map(parse_datetime_ms)
        .filter(|&ms| ms > 0);

    let metrics = SessionUsageMetrics {
        input_tokens: analysis.cache.cumulative_input_tokens,
        cached_tokens: analysis.cache.cumulative_cached_tokens,
        output_tokens: analysis.cache.cumulative_output_tokens,
        compaction_count: analysis.compactions.len(),
        model_key,
        model_label,
        last_step_ms,
    };

    if !analysis.db_busy {
        cache.entries.insert(
            conv_id.clone(),
            CachedSessionUsage {
                transcript_fp: tfp,
                db_fp: dfp,
                metrics: metrics.clone(),
            },
        );
    }

    Some(metrics)
}

fn resolve_model_display(
    model_from_db: bool,
    analysis_model_id: &str,
    analysis_model_label: &str,
    summary_model_label: &str,
) -> (String, String) {
    if model_from_db && !analysis_model_id.is_empty() {
        return (
            analysis_model_id.to_string(),
            analysis_model_label.to_string(),
        );
    }
    let summary_trimmed = summary_model_label.trim();
    if !summary_trimmed.is_empty() && summary_trimmed != "Default model" {
        let slug = normalize_model_slug(summary_trimmed);
        let key = if slug.is_empty() {
            summary_trimmed.to_lowercase()
        } else {
            slug
        };
        return (key, summary_trimmed.to_string());
    }
    if !analysis_model_id.is_empty() {
        return (
            analysis_model_id.to_string(),
            analysis_model_label.to_string(),
        );
    }
    ("unknown".to_string(), "不明".to_string())
}

fn resolve_project_display(summary: &SessionSummary) -> (String, String) {
    let key = if summary.project_key.trim().is_empty() {
        "none".to_string()
    } else {
        summary.project_key.trim().to_string()
    };
    let label = if !summary.project_label.trim().is_empty() {
        summary.project_label.trim().to_string()
    } else if key == "none" {
        "未設定".to_string()
    } else {
        key.clone()
    };
    (key, label)
}

fn add_breakdown(
    map: &mut HashMap<String, UsageBreakdownRow>,
    key: &str,
    label: &str,
    metrics: &SessionUsageMetrics,
) {
    let row = map
        .entry(key.to_string())
        .or_insert_with(|| UsageBreakdownRow {
            key: key.to_string(),
            label: label.to_string(),
            ..Default::default()
        });
    row.session_count += 1;
    row.input_tokens = row.input_tokens.saturating_add(metrics.input_tokens);
    row.cached_tokens = row.cached_tokens.saturating_add(metrics.cached_tokens);
    row.output_tokens = row.output_tokens.saturating_add(metrics.output_tokens);
    row.compaction_count = row
        .compaction_count
        .saturating_add(metrics.compaction_count);
}

fn sort_breakdown(rows: &mut [UsageBreakdownRow]) {
    rows.sort_by(|a, b| {
        let a_total = a.input_tokens.saturating_add(a.output_tokens);
        let b_total = b.input_tokens.saturating_add(b.output_tokens);
        b_total.cmp(&a_total).then_with(|| a.key.cmp(&b.key))
    });
}
