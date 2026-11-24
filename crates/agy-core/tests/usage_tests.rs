use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use agy_core::paths::{GeminiPaths, Surface};
use agy_core::sessions::{SessionIndex, SessionSummary, load_session_index};
use agy_core::usage::{
    DEFAULT_MAX_USAGE_SESSIONS, UsageCache, UsagePeriod, aggregate_usage, aggregate_usage_at,
};
use chrono::{DateTime, Local, TimeZone};
use rusqlite::Connection;
use tempfile::TempDir;

fn encode_varint(mut v: u64, buf: &mut Vec<u8>) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf.push(b);
        if v == 0 {
            break;
        }
    }
}

fn encode_bytes_field(field: u64, data: &[u8], buf: &mut Vec<u8>) {
    encode_varint((field << 3) | 2, buf);
    encode_varint(data.len() as u64, buf);
    buf.extend_from_slice(data);
}

fn make_step_metadata_with_model(model_slug: &str) -> Vec<u8> {
    let mut model_msg = Vec::new();
    encode_bytes_field(12, model_slug.as_bytes(), &mut model_msg);
    let mut step_meta = Vec::new();
    encode_bytes_field(24, &model_msg, &mut step_meta);
    step_meta
}

fn write_conversation_db_model(app_data: &Path, conv_id: &str, model_slug: &str) {
    let conv_dir = app_data.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();
    let db_path = conv_dir.join(format!("{conv_id}.db"));
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
        [],
    )
    .unwrap();
    let meta = make_step_metadata_with_model(model_slug);
    conn.execute("INSERT INTO steps (idx, metadata) VALUES (0, ?1)", [meta])
        .unwrap();
}

fn write_transcript(app_data: &Path, conv_id: &str, lines: &[serde_json::Value]) {
    let logs_dir = app_data
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();
    let path = logs_dir.join("transcript.jsonl");
    let mut file = std::fs::File::create(path).unwrap();
    for line in lines {
        writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
}

fn planner_step(
    idx: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
) -> serde_json::Value {
    serde_json::json!({
        "step_index": idx,
        "source": "MODEL",
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": "2026-11-24T10:00:00Z",
        "input_tokens": input_tokens,
        "cache_read_tokens": cache_read_tokens,
        "output_tokens": output_tokens
    })
}

fn checkpoint_step(idx: u64) -> serde_json::Value {
    serde_json::json!({
        "step_index": idx,
        "source": "SYSTEM",
        "type": "CHECKPOINT",
        "status": "DONE",
        "created_at": "2026-11-24T10:05:00Z",
        "content": "Compacted checkpoint summary"
    })
}

fn local_ms(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> i64 {
    Local
        .with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()
        .unwrap()
        .timestamp_millis()
}

fn local_dt(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()
        .unwrap()
}

fn make_summary(
    conv_id: &str,
    surface: Surface,
    app_data: &Path,
    project_key: &str,
    project_label: &str,
    effective_mtime_ms: i64,
) -> SessionSummary {
    SessionSummary {
        conversation_id: conv_id.to_string(),
        surface,
        app_data_dir: app_data.to_path_buf(),
        title: conv_id.to_string(),
        preview: String::new(),
        display_title: conv_id.to_string(),
        agent_name: String::new(),
        parent_conversation_id: String::new(),
        root_session_id: conv_id.to_string(),
        root_title: conv_id.to_string(),
        is_subagent: false,
        nesting_depth: 0,
        project_id: String::new(),
        project_key: project_key.to_string(),
        project_label: project_label.to_string(),
        workspace_keys: Vec::new(),
        last_modified_ms: effective_mtime_ms,
        effective_mtime_ms,
        has_transcript: true,
        has_db: false,
        model_label: String::new(),
    }
}

#[test]
fn usage_01_period_labels_and_constants() {
    assert_eq!(UsagePeriod::default(), UsagePeriod::Last7Days);
    assert_eq!(UsagePeriod::Today.label(), "今日");
    assert_eq!(UsagePeriod::Last7Days.label(), "7日間");
    assert_eq!(UsagePeriod::Last30Days.label(), "30日間");
    assert_eq!(UsagePeriod::AllTime.label(), "全期間");
    assert_eq!(DEFAULT_MAX_USAGE_SESSIONS, 500);
}

#[test]
fn usage_02_period_boundaries_local_dates() {
    let tmp = TempDir::new().unwrap();
    let app_data = tmp.path().join("antigravity");

    let sessions = [
        (
            "11111111-1111-1111-1111-111111111111",
            local_ms(2026, 11, 24, 0, 1, 0),
            100u64,
        ), // Today
        (
            "22222222-2222-2222-2222-222222222222",
            local_ms(2026, 11, 23, 23, 59, 0),
            200u64,
        ), // Yesterday (in 7d)
        (
            "33333333-3333-3333-3333-333333333333",
            local_ms(2026, 11, 18, 0, 1, 0),
            300u64,
        ), // 6 days back (inclusive start of 7d)
        (
            "44444444-4444-4444-4444-444444444444",
            local_ms(2026, 11, 17, 23, 59, 0),
            400u64,
        ), // 7 days back (excluded from 7d, in 30d)
        (
            "55555555-5555-5555-5555-555555555555",
            local_ms(2026, 10, 26, 0, 1, 0),
            500u64,
        ), // 29 days back (inclusive start of 30d)
        (
            "66666666-6666-6666-6666-666666666666",
            local_ms(2026, 10, 25, 23, 59, 0),
            600u64,
        ), // 30 days back (excluded from 30d, in AllTime)
        (
            "77777777-7777-7777-7777-777777777777",
            local_ms(2026, 11, 25, 0, 1, 0),
            700u64,
        ), // Future relative to now_local (excluded from bounded periods, in AllTime)
    ];

    let mut by_id = HashMap::new();
    for (id, ms, uncached) in sessions {
        write_transcript(&app_data, id, &[planner_step(0, uncached, 0, 10)]);
        by_id.insert(
            id.to_string(),
            make_summary(id, Surface::App, &app_data, "proj", "Project", ms),
        );
    }
    let index = SessionIndex {
        by_id,
        ..Default::default()
    };

    let now = local_dt(2026, 11, 24, 14, 0, 0);
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    let today_report = aggregate_usage_at(
        &index,
        UsagePeriod::Today,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(today_report.total_sessions, 1);
    assert_eq!(today_report.analyzed_sessions, 1);
    assert_eq!(today_report.total_input_tokens, 100);
    assert_eq!(today_report.daily.len(), 1);
    assert_eq!(today_report.daily[0].date_ymd, "2026-11-24");

    let d7_report = aggregate_usage_at(
        &index,
        UsagePeriod::Last7Days,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(d7_report.total_sessions, 3);
    assert_eq!(d7_report.analyzed_sessions, 3);
    assert_eq!(d7_report.total_input_tokens, 100 + 200 + 300);

    let d30_report = aggregate_usage_at(
        &index,
        UsagePeriod::Last30Days,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(d30_report.total_sessions, 5);
    assert_eq!(d30_report.analyzed_sessions, 5);
    assert_eq!(d30_report.total_input_tokens, 100 + 200 + 300 + 400 + 500);

    let all_report = aggregate_usage_at(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(all_report.total_sessions, 7);
    assert_eq!(all_report.analyzed_sessions, 7);
    assert_eq!(
        all_report.total_input_tokens,
        100 + 200 + 300 + 400 + 500 + 600 + 700
    );
}

#[test]
fn usage_03_daily_model_project_surface_aggregation_and_tiebreak_sorting() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("antigravity");
    let cli_dir = tmp.path().join("antigravity-cli");
    let ide_dir = tmp.path().join("antigravity-ide");

    // Session A (App, proj-b, claude-opus-4-6, day 24): input=1000 (200 uncached + 800 cached), out=200 -> total=1200, 1 compaction
    let id_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    write_conversation_db_model(&app_dir, id_a, "claude-opus-4-6");
    write_transcript(
        &app_dir,
        id_a,
        &[
            planner_step(0, 120, 400, 100),
            checkpoint_step(1),
            planner_step(2, 80, 400, 100),
        ],
    );

    // Session B (Cli, proj-a, gemini-3.1-pro, day 23): input=900 (300 uncached + 600 cached), out=300 -> total=1200 (ties with A!)
    let id_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    write_conversation_db_model(&cli_dir, id_b, "gemini-3.1-pro");
    write_transcript(&cli_dir, id_b, &[planner_step(0, 300, 600, 300)]);

    // Session C (Ide, none project, unknown model, day 23): 0 tokens -> tests zero input and unknown handling
    let id_c = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    write_transcript(
        &ide_dir,
        id_c,
        &[serde_json::json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "content": "hello"
        })],
    );

    // Session D (App, proj-a, missing transcript -> skipped)
    let id_d = "dddddddd-dddd-dddd-dddd-dddddddddddd";

    let mut by_id = HashMap::new();
    by_id.insert(
        id_a.to_string(),
        make_summary(
            id_a,
            Surface::App,
            &app_dir,
            "proj-b",
            "Project B",
            local_ms(2026, 11, 24, 10, 0, 0),
        ),
    );
    by_id.insert(
        id_b.to_string(),
        make_summary(
            id_b,
            Surface::Cli,
            &cli_dir,
            "proj-a",
            "Project A",
            local_ms(2026, 11, 23, 15, 0, 0),
        ),
    );
    by_id.insert(
        id_c.to_string(),
        make_summary(
            id_c,
            Surface::Ide,
            &ide_dir,
            "none",
            "",
            local_ms(2026, 11, 23, 9, 0, 0),
        ),
    );
    by_id.insert(
        id_d.to_string(),
        make_summary(
            id_d,
            Surface::App,
            &app_dir,
            "proj-a",
            "Project A",
            local_ms(2026, 11, 24, 11, 0, 0),
        ),
    );

    let index = SessionIndex {
        by_id,
        ..Default::default()
    };
    let now = local_dt(2026, 11, 24, 18, 0, 0);
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    let report = aggregate_usage_at(
        &index,
        UsagePeriod::Last7Days,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();

    assert_eq!(report.period, UsagePeriod::Last7Days);
    assert_eq!(report.total_sessions, 4);
    assert_eq!(report.analyzed_sessions, 3);
    assert_eq!(report.skipped_sessions, 1);
    assert!(!report.truncated);

    assert_eq!(report.total_input_tokens, 1900);
    assert_eq!(report.total_cached_tokens, 1400);
    assert_eq!(report.total_output_tokens, 500);
    assert_eq!(report.total_compactions, 1);
    let expected_ratio = 1400.0_f64 / 1900.0_f64;
    assert!((report.cache_hit_ratio - expected_ratio).abs() < 1e-12);

    // Daily sorted ascending by date_ymd
    assert_eq!(report.daily.len(), 2);
    assert_eq!(report.daily[0].date_ymd, "2026-11-23");
    assert_eq!(report.daily[0].session_count, 2);
    assert_eq!(report.daily[0].input_tokens, 900);
    assert_eq!(report.daily[0].cached_tokens, 600);
    assert_eq!(report.daily[0].output_tokens, 300);
    assert_eq!(report.daily[0].compaction_count, 0);

    assert_eq!(report.daily[1].date_ymd, "2026-11-24");
    assert_eq!(report.daily[1].session_count, 1);
    assert_eq!(report.daily[1].input_tokens, 1000);
    assert_eq!(report.daily[1].cached_tokens, 800);
    assert_eq!(report.daily[1].output_tokens, 200);
    assert_eq!(report.daily[1].compaction_count, 1);

    // Breakdown tie-breaking: A and B both have input+output = 1200.
    // Model: "claude-opus-4-6" < "gemini-3.1-pro", then "unknown" (0)
    assert_eq!(report.by_model.len(), 3);
    assert_eq!(report.by_model[0].key, "claude-opus-4-6");
    assert_eq!(report.by_model[0].label, "Claude Opus 4.6");
    assert_eq!(report.by_model[1].key, "gemini-3.1-pro");
    assert_eq!(report.by_model[2].key, "unknown");

    // Project: "proj-a" (1200) < "proj-b" (1200), then "none" (0)
    assert_eq!(report.by_project.len(), 3);
    assert_eq!(report.by_project[0].key, "proj-a");
    assert_eq!(report.by_project[0].label, "Project A");
    assert_eq!(report.by_project[1].key, "proj-b");
    assert_eq!(report.by_project[1].label, "Project B");
    assert_eq!(report.by_project[2].key, "none");

    // Surface: "app" (1200) < "cli" (1200), then "ide" (0)
    assert_eq!(report.by_surface.len(), 3);
    assert_eq!(report.by_surface[0].key, "app");
    assert_eq!(report.by_surface[1].key, "cli");
    assert_eq!(report.by_surface[2].key, "ide");

    // Zero-token session alone yields cache_hit_ratio == 0.0
    let mut only_c = HashMap::new();
    only_c.insert(id_c.to_string(), index.by_id[id_c].clone());
    let zero_index = SessionIndex {
        by_id: only_c,
        ..Default::default()
    };
    let zero_report = aggregate_usage_at(
        &zero_index,
        UsagePeriod::Last7Days,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(zero_report.cache_hit_ratio, 0.0);
}

#[test]
fn usage_04_fingerprint_cache_reuse_and_invalidation() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("antigravity");
    let conv_id = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";

    let initial_line = serde_json::to_string(&planner_step(0, 100, 50, 20)).unwrap() + "\n";
    let logs_dir = app_dir
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();
    let transcript_path = logs_dir.join("transcript.jsonl");
    std::fs::write(&transcript_path, initial_line.as_bytes()).unwrap();
    let orig_mtime = std::fs::metadata(&transcript_path)
        .unwrap()
        .modified()
        .unwrap();

    let mut by_id = HashMap::new();
    by_id.insert(
        conv_id.to_string(),
        make_summary(
            conv_id,
            Surface::App,
            &app_dir,
            "proj",
            "Proj",
            local_ms(2026, 11, 24, 12, 0, 0),
        ),
    );
    let index = SessionIndex {
        by_id,
        ..Default::default()
    };
    let now = local_dt(2026, 11, 24, 18, 0, 0);
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    let first = aggregate_usage_at(
        &index,
        UsagePeriod::Today,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(first.total_input_tokens, 150);
    assert_eq!(first.total_output_tokens, 20);
    assert_eq!(cache.len(), 1);

    // Overwrite with different tokens of identical byte length and identical mtime:
    // cache hit must preserve original values without re-reading disk.
    let same_len_line = serde_json::to_string(&planner_step(0, 999, 11, 88)).unwrap() + "\n";
    assert_eq!(same_len_line.len(), initial_line.len());
    {
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&transcript_path)
            .unwrap();
        f.write_all(same_len_line.as_bytes()).unwrap();
        f.sync_all().unwrap();
        f.set_modified(orig_mtime).unwrap();
    }

    let cached_second = aggregate_usage_at(
        &index,
        UsagePeriod::Today,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(cached_second.total_input_tokens, 150);
    assert_eq!(cached_second.total_output_tokens, 20);

    // Change byte size to invalidate TranscriptFingerprint -> re-reads transcript.
    let expanded_line = serde_json::to_string(&planner_step(0, 9999, 100, 88)).unwrap() + "\n";
    std::fs::write(&transcript_path, expanded_line.as_bytes()).unwrap();

    let refreshed = aggregate_usage_at(
        &index,
        UsagePeriod::Today,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(refreshed.total_input_tokens, 10099);
    assert_eq!(refreshed.total_cached_tokens, 100);
    assert_eq!(refreshed.total_output_tokens, 88);

    // Delete transcript -> skipped even when cache previously held the conv_id.
    std::fs::remove_file(&transcript_path).unwrap();
    let after_delete = aggregate_usage_at(
        &index,
        UsagePeriod::Today,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
        now,
    )
    .unwrap();
    assert_eq!(after_delete.total_sessions, 1);
    assert_eq!(after_delete.analyzed_sessions, 0);
    assert_eq!(after_delete.skipped_sessions, 1);
}

#[test]
fn usage_05_max_sessions_cap_and_truncation() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("antigravity");

    let mut by_id = HashMap::new();
    for i in 1..=5u64 {
        let id = format!("{i:08x}-0000-0000-0000-000000000000");
        write_transcript(&app_dir, &id, &[planner_step(0, i * 100, 0, i * 10)]);
        by_id.insert(
            id.clone(),
            make_summary(
                &id,
                Surface::App,
                &app_dir,
                "proj",
                "Proj",
                local_ms(2026, 11, 24, 10, i as u32, 0),
            ),
        );
    }
    let index = SessionIndex {
        by_id,
        ..Default::default()
    };
    let now = local_dt(2026, 11, 24, 18, 0, 0);
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    // Newest 3 are i=5, 4, 3 -> 500 + 400 + 300 = 1200 input, 50 + 40 + 30 = 120 output
    let capped =
        aggregate_usage_at(&index, UsagePeriod::Today, 3, &mut cache, &cancel, now).unwrap();
    assert_eq!(capped.total_sessions, 5);
    assert_eq!(capped.analyzed_sessions, 3);
    assert_eq!(capped.skipped_sessions, 0);
    assert!(capped.truncated);
    assert_eq!(capped.total_input_tokens, 1200);
    assert_eq!(capped.total_output_tokens, 120);

    let exact =
        aggregate_usage_at(&index, UsagePeriod::Today, 5, &mut cache, &cancel, now).unwrap();
    assert_eq!(exact.total_sessions, 5);
    assert_eq!(exact.analyzed_sessions, 5);
    assert!(!exact.truncated);

    let zero_cap =
        aggregate_usage_at(&index, UsagePeriod::Today, 0, &mut cache, &cancel, now).unwrap();
    assert_eq!(zero_cap.total_sessions, 5);
    assert_eq!(zero_cap.analyzed_sessions, 0);
    assert!(zero_cap.truncated);
}

#[test]
fn usage_06_cancellation_returns_none() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("antigravity");
    let conv_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
    write_transcript(&app_dir, conv_id, &[planner_step(0, 100, 0, 10)]);

    let mut by_id = HashMap::new();
    by_id.insert(
        conv_id.to_string(),
        make_summary(
            conv_id,
            Surface::App,
            &app_dir,
            "proj",
            "Proj",
            Local::now().timestamp_millis(),
        ),
    );
    let index = SessionIndex {
        by_id,
        ..Default::default()
    };
    let cancel = AtomicBool::new(true);
    let mut cache = UsageCache::default();

    assert!(
        aggregate_usage(
            &index,
            UsagePeriod::AllTime,
            DEFAULT_MAX_USAGE_SESSIONS,
            &mut cache,
            &cancel
        )
        .is_none()
    );
    assert!(
        aggregate_usage_at(
            &index,
            UsagePeriod::Today,
            DEFAULT_MAX_USAGE_SESSIONS,
            &mut cache,
            &cancel,
            Local::now()
        )
        .is_none()
    );

    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let report = aggregate_usage(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
    )
    .unwrap();
    assert_eq!(report.analyzed_sessions, 1);
}

#[test]
fn usage_07_load_session_index_integration() {
    let tmp = TempDir::new().unwrap();
    let gemini_dir = tmp.path();
    let app_dir = gemini_dir.join("antigravity");
    let cli_dir = gemini_dir.join("antigravity-cli");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&cli_dir).unwrap();

    let proj_path = gemini_dir.join("repo-alpha");
    std::fs::create_dir_all(&proj_path).unwrap();
    let projects_json = serde_json::json!({
        "projects": {
            proj_path.to_string_lossy().to_string(): "Repo Alpha"
        }
    });
    std::fs::write(
        gemini_dir.join("projects.json"),
        serde_json::to_string(&projects_json).unwrap(),
    )
    .unwrap();

    let conv_app = "12345678-1234-1234-1234-1234567890ab";
    let ws_json = serde_json::to_string(&vec![proj_path.to_string_lossy().to_string()]).unwrap();
    let summary_db = app_dir.join("conversation_summaries.db");
    let conn = Connection::open(&summary_db).unwrap();
    conn.execute(
        "CREATE TABLE conversation_summaries (
            conversation_id TEXT PRIMARY KEY,
            title TEXT,
            preview TEXT,
            workspace_uris TEXT,
            project_id TEXT,
            agent_name TEXT,
            parent_conversation_id TEXT,
            nesting_depth INTEGER,
            last_modified_time TEXT
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversation_summaries VALUES (?1, 'App Session', '', ?2, 'pid-alpha', '', '', 0, '2026-11-24T08:00:00Z')",
        [conv_app, &ws_json],
    )
    .unwrap();
    drop(conn);

    write_conversation_db_model(&app_dir, conv_app, "claude-sonnet-4-6");
    write_transcript(&app_dir, conv_app, &[planner_step(0, 250, 750, 50)]);

    let conv_cli = "abcdef01-2345-6789-abcd-ef0123456789";
    write_transcript(&cli_dir, conv_cli, &[planner_step(0, 400, 100, 40)]);

    let paths = GeminiPaths::from_gemini_dir(gemini_dir);
    let index = load_session_index(&paths);
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    let report = aggregate_usage(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
    )
    .unwrap();

    assert_eq!(report.total_sessions, 2);
    assert_eq!(report.analyzed_sessions, 2);
    assert_eq!(report.skipped_sessions, 0);
    assert_eq!(report.total_input_tokens, 1500);
    assert_eq!(report.total_cached_tokens, 850);
    assert_eq!(report.total_output_tokens, 90);
    assert!(report.by_project.iter().any(|r| r.label == "Repo Alpha"));
    assert!(report.by_surface.iter().any(|r| r.key == "app"));
    assert!(report.by_surface.iter().any(|r| r.key == "cli"));
    assert!(
        report
            .by_model
            .iter()
            .any(|r| r.key == "claude-sonnet-4-6" && r.label == "Claude Sonnet 4.6")
    );
}

#[test]
fn usage_08_aggregate_usage_with_mock_customizations_on_disk() {
    let tmp = TempDir::new().unwrap();
    let gemini_dir = tmp.path().join(".gemini");
    let app_dir = gemini_dir.join("antigravity");
    let cli_dir = gemini_dir.join("antigravity-cli");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&cli_dir).unwrap();

    // Populate oversized / noisy customization files under .gemini.
    // Period usage aggregation bypasses customization scanning and stays exact.
    let mcp_dir = app_dir.join("mcp").join("heavy-mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(mcp_dir.join("instructions.md"), "instruction ".repeat(5000)).unwrap();
    std::fs::write(app_dir.join("mcp_config.json"), "{ invalid json").unwrap();

    let builtin_skill = app_dir.join("builtin").join("skills").join("heavy-skill");
    std::fs::create_dir_all(&builtin_skill).unwrap();
    std::fs::write(builtin_skill.join("SKILL.md"), "skill ".repeat(2000)).unwrap();

    let plugin_rules = gemini_dir
        .join("config")
        .join("plugins")
        .join("demo-plugin")
        .join("rules");
    std::fs::create_dir_all(&plugin_rules).unwrap();
    std::fs::write(plugin_rules.join("always.md"), "rule ".repeat(5000)).unwrap();

    let id_app = "11112222-3333-4444-5555-666677778888";
    write_conversation_db_model(&app_dir, id_app, "claude-opus-4-6");
    write_transcript(
        &app_dir,
        id_app,
        &[
            planner_step(0, 200, 300, 50),
            checkpoint_step(1),
            planner_step(2, 100, 400, 75),
        ],
    );

    let id_cli = "99998888-7777-6666-5555-444433332222";
    write_conversation_db_model(&cli_dir, id_cli, "gemini-3.1-pro");
    write_transcript(&cli_dir, id_cli, &[planner_step(0, 300, 700, 125)]);

    let paths = GeminiPaths::from_gemini_dir(&gemini_dir);
    let index = load_session_index(&paths);
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    let report = aggregate_usage(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
    )
    .unwrap();

    assert_eq!(report.total_sessions, 2);
    assert_eq!(report.analyzed_sessions, 2);
    assert_eq!(report.skipped_sessions, 0);
    assert_eq!(report.total_input_tokens, 2000);
    assert_eq!(report.total_cached_tokens, 1400);
    assert_eq!(report.total_output_tokens, 250);
    assert_eq!(report.total_compactions, 1);
    assert!((report.cache_hit_ratio - 0.7).abs() < 1e-12);
    assert_eq!(report.by_surface.len(), 2);
    assert_eq!(report.by_model.len(), 2);
    assert!(
        report.by_model.iter().any(|r| r.key == "claude-opus-4-6"
            && r.input_tokens == 1000
            && r.output_tokens == 125)
    );
    assert!(
        report
            .by_model
            .iter()
            .any(|r| r.key == "gemini-3.1-pro" && r.input_tokens == 1000 && r.output_tokens == 125)
    );
}

#[test]
fn usage_09_model_breakdown_ignores_pbtxt_fallback_when_db_model_missing() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join(".gemini").join("antigravity");
    std::fs::create_dir_all(&app_dir).unwrap();
    let pbtxt_path = app_dir.join("antigravity_state.pbtxt");
    std::fs::write(
        &pbtxt_path,
        "last_selected_agent_model: MODEL_PLACEHOLDER_M26\n",
    )
    .unwrap();

    // Session without DB model info and empty summary label
    let id_missing_db = "10101010-2020-3030-4040-505050505050";
    write_transcript(&app_dir, id_missing_db, &[planner_step(0, 100, 300, 40)]);

    // Session without DB model info and "Default model" summary label
    let id_default_label = "20202020-3030-4040-5050-606060606060";
    write_transcript(&app_dir, id_default_label, &[planner_step(0, 200, 400, 60)]);

    // Control session with DB model info
    let id_with_db = "30303030-4040-5050-6060-707070707070";
    write_conversation_db_model(&app_dir, id_with_db, "gemini-3.1-pro");
    write_transcript(&app_dir, id_with_db, &[planner_step(0, 50, 150, 20)]);

    let mut s1 = make_summary(
        id_missing_db,
        Surface::App,
        &app_dir,
        "path:c:/repo/alpha",
        "alpha",
        1_000,
    );
    s1.model_label = String::new();

    let mut s2 = make_summary(
        id_default_label,
        Surface::App,
        &app_dir,
        "path:c:/repo/alpha",
        "alpha",
        2_000,
    );
    s2.model_label = "Default model".to_string();

    let s3 = make_summary(
        id_with_db,
        Surface::App,
        &app_dir,
        "path:c:/repo/alpha",
        "alpha",
        3_000,
    );

    let mut by_id = HashMap::new();
    by_id.insert(id_missing_db.to_string(), s1);
    by_id.insert(id_default_label.to_string(), s2);
    by_id.insert(id_with_db.to_string(), s3);

    let index = SessionIndex {
        by_id,
        ..Default::default()
    };
    let cancel = AtomicBool::new(false);
    let mut cache = UsageCache::default();

    let report1 = aggregate_usage(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
    )
    .unwrap();

    assert_eq!(report1.by_model.len(), 2);
    let unknown1 = report1
        .by_model
        .iter()
        .find(|r| r.key == "unknown")
        .expect("unknown bucket must exist");
    assert_eq!(unknown1.label, "不明");
    assert_eq!(unknown1.session_count, 2);
    assert_eq!(unknown1.input_tokens, 1000);
    assert_eq!(unknown1.cached_tokens, 700);
    assert_eq!(unknown1.output_tokens, 100);
    assert!(
        !report1
            .by_model
            .iter()
            .any(|r| r.key.contains("claude") || r.label.contains("Claude")),
        "pbtxt fallback model must not leak into period model breakdown"
    );

    // Change antigravity_state.pbtxt and re-aggregate both with warm cache and fresh cache.
    std::fs::write(
        &pbtxt_path,
        "last_selected_agent_model: MODEL_PLACEHOLDER_M35\n",
    )
    .unwrap();

    let report_cached = aggregate_usage(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut cache,
        &cancel,
    )
    .unwrap();
    assert_eq!(report_cached.by_model, report1.by_model);

    let mut fresh_cache = UsageCache::default();
    let report_fresh = aggregate_usage(
        &index,
        UsagePeriod::AllTime,
        DEFAULT_MAX_USAGE_SESSIONS,
        &mut fresh_cache,
        &cancel,
    )
    .unwrap();
    assert_eq!(report_fresh.by_model, report1.by_model);
}
