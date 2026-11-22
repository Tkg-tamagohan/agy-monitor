use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use agy_core::CoreError;
use agy_core::analyzer::{
    ContextBuckets, analyze_conversation, est_text_tokens, reconcile_active_buckets,
};
use agy_core::fingerprint::{DbFingerprint, FileStamp, TranscriptFingerprint};
use agy_core::models::{
    detect_conversation_model, detect_conversation_model_label_from_db, format_model_label,
    format_model_short_label, is_concrete_model_slug, lookup_model_id,
    map_placeholder_enum_to_slug, normalize_model_slug, resolve_model_context_limit,
};
use agy_core::monitor::{MonitorCommand, MonitorConfig, MonitorHandle};
use agy_core::paths::{GeminiPaths, Surface, normalize_file_uri, normalize_path_key};
use agy_core::proto::{decode_fields, proto_bytes, proto_nested, proto_num, proto_text};
use agy_core::sessions::{load_session_index, parse_datetime_ms, truncate_chars};
use agy_core::sqlite::{open_readonly, with_readonly_retry};
use rusqlite::{Connection, OpenFlags};
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

fn encode_tag(field: u64, wire: u64, buf: &mut Vec<u8>) {
    encode_varint((field << 3) | wire, buf);
}

fn encode_bytes_field(field: u64, data: &[u8], buf: &mut Vec<u8>) {
    encode_tag(field, 2, buf);
    encode_varint(data.len() as u64, buf);
    buf.extend_from_slice(data);
}

fn encode_varint_field(field: u64, v: u64, buf: &mut Vec<u8>) {
    encode_tag(field, 0, buf);
    encode_varint(v, buf);
}

#[test]
fn proto_01_decode_valid_nested_and_utf8() {
    let mut inner = Vec::new();
    encode_varint_field(1, 1026, &mut inner);
    encode_bytes_field(12, b"  claude-opus-4-6  ", &mut inner);

    let mut outer = Vec::new();
    encode_bytes_field(24, &inner, &mut outer);
    encode_tag(2, 1, &mut outer);
    outer.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // fixed64 skipped cleanly
    encode_tag(3, 5, &mut outer);
    outer.extend_from_slice(&[9, 10, 11, 12]); // fixed32 skipped cleanly
    encode_varint_field(4, 42, &mut outer);

    let map = decode_fields(&outer);
    assert_eq!(proto_num(&map, 4), 42);
    let nested = proto_nested(&map, 24);
    assert_eq!(proto_num(&nested, 1), 1026);
    assert_eq!(proto_text(&nested, 12), "claude-opus-4-6");
}

#[test]
fn proto_02_truncated_and_malformed_never_panic() {
    let cases: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x80],                   // truncated varint tag
        vec![0x00],                   // field number 0
        vec![0x08, 0x80],             // truncated varint value
        vec![0x12, 0x10, 0x01, 0x02], // length 16 > remaining 2
        vec![
            0x12, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02,
        ], // >64-bit varint
        vec![0x1b],                   // wire type 3 (group start)
        vec![0x1f],                   // wire type 7 (invalid)
        vec![0x09, 0x01, 0x02],       // truncated fixed64
        vec![0x0d, 0x01, 0x02],       // truncated fixed32
    ];

    for raw in cases {
        let map = decode_fields(&raw);
        let _ = proto_num(&map, 1);
        let _ = proto_bytes(&map, 1);
        let _ = proto_text(&map, 1);
    }

    // Invalid UTF-8 in text field returns empty string safely
    let mut bad_utf8 = Vec::new();
    encode_bytes_field(5, &[0xff, 0xfe, 0xfd], &mut bad_utf8);
    let map = decode_fields(&bad_utf8);
    assert_eq!(proto_text(&map, 5), "");
}

#[test]
fn model_01_mapping_normalization_limits_and_pbtxt_fallback() {
    assert_eq!(lookup_model_id(1026), Some("claude-opus-4-6"));
    assert_eq!(lookup_model_id(1667), Some("gemini-pro-orchestrator"));
    assert_eq!(
        normalize_model_slug("Claude 4.5 Sonnet (High)"),
        "claude-sonnet-4-5-high"
    );
    assert_eq!(
        format_model_label("claude-sonnet-4-5-high"),
        "Claude Sonnet 4.5 (High)"
    );
    assert!(is_concrete_model_slug("claude-opus-4-6"));
    assert!(!is_concrete_model_slug("gemini-3-flash-agent-a"));
    assert!(!is_concrete_model_slug("gemini-4-argon-b"));

    // Context limits: agy_max_token_limit prioritized over default
    let info_explicit = resolve_model_context_limit("claude-opus-4-6", 180_000);
    assert_eq!(info_explicit.context_limit, 180_000);
    assert_eq!(info_explicit.limit_source, "antigravity");

    let info_default = resolve_model_context_limit("claude-opus-4-6", 0);
    assert_eq!(info_default.model_max_tokens, 1_000_000);
    assert_eq!(info_default.context_limit, 256_000);
    assert_eq!(info_default.limit_source, "antigravity-default");

    let info_orch = resolve_model_context_limit("gemini-pro-orchestrator", 0);
    assert_eq!(info_orch.context_limit, 512_000);

    // pbtxt placeholder fallback
    assert_eq!(
        map_placeholder_enum_to_slug("MODEL_PLACEHOLDER_M26"),
        "claude-opus-4-6"
    );

    let tmp = TempDir::new().unwrap();
    let app_data = tmp.path().join("antigravity");
    std::fs::create_dir_all(app_data.join("conversations")).unwrap();
    let pbtxt = app_data.join("antigravity_state.pbtxt");
    std::fs::write(&pbtxt, "last_selected_agent_model: MODEL_PLACEHOLDER_M35\n").unwrap();

    let detected =
        detect_conversation_model(&app_data, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", None);
    assert_eq!(detected.info.model_id, "claude-sonnet-4-6");
    assert!(!detected.db_busy);
    assert!(!detected.model_from_db);

    // executor_metadata limit overrides fallback when DB is present
    let conv_id = "11111111-2222-3333-4444-555555555555";
    let db_path = app_data.join("conversations").join(format!("{conv_id}.db"));
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
         CREATE TABLE executor_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
    )
    .unwrap();

    let mut step_model = Vec::new();
    encode_varint_field(1, 1026, &mut step_model);
    let mut step_meta = Vec::new();
    encode_bytes_field(24, &step_model, &mut step_meta);
    conn.execute(
        "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
        [&step_meta],
    )
    .unwrap();

    let mut cp = Vec::new();
    encode_varint_field(5, 320_000, &mut cp);
    let mut cc = Vec::new();
    encode_bytes_field(2, &cp, &mut cc);
    let mut exec_root = Vec::new();
    encode_bytes_field(10, &cc, &mut exec_root);
    conn.execute(
        "INSERT INTO executor_metadata (idx, data) VALUES (1, ?1)",
        [&exec_root],
    )
    .unwrap();
    drop(conn);

    let detected_db = detect_conversation_model(&app_data, conv_id, None);
    assert_eq!(detected_db.info.model_id, "claude-opus-4-6");
    assert_eq!(detected_db.info.context_limit, 320_000);
    assert_eq!(detected_db.info.limit_source, "antigravity");
    assert!(detected_db.model_from_db);
}

#[test]
fn path_01_windows_file_uris_surfaces_and_projects_json() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    std::fs::create_dir_all(gemini.join("antigravity")).unwrap();
    std::fs::create_dir_all(gemini.join("antigravity-cli")).unwrap();
    std::fs::create_dir_all(gemini.join("antigravity-ide")).unwrap();
    std::fs::create_dir_all(gemini.join("antigravity-backup")).unwrap();

    std::fs::write(
        gemini.join("projects.json"),
        r#"{"projects":{"e:\\documents\\code\\devin-plugins":"devin-plugins","C:\\Users\\test\\my repo":"my-repo"}}"#,
    )
    .unwrap();

    let paths = GeminiPaths::from_gemini_dir(&gemini);
    let roots = paths.app_data_roots();
    assert_eq!(roots.len(), 4);
    assert_eq!(roots[0].surface, Surface::App);
    assert_eq!(roots[1].surface, Surface::Cli);
    assert_eq!(roots[2].surface, Surface::Ide);
    assert_eq!(roots[3].surface, Surface::Backup);

    let proj = paths.load_projects_json();
    assert_eq!(
        proj.get(&normalize_path_key(
            "file:///e%3A/Documents/code/devin-plugins"
        ))
        .map(String::as_str),
        Some("devin-plugins")
    );
    assert_eq!(
        proj.get(&normalize_path_key(r"file:///C:\Users\test\my%20repo"))
            .map(String::as_str),
        Some("my-repo")
    );
    assert_eq!(
        proj.get(&normalize_path_key(r"file://C:\Users\test\my repo"))
            .map(String::as_str),
        Some("my-repo")
    );
    assert!(normalize_file_uri(r"file:///C:\Users\test\repo").is_some());
    assert!(normalize_file_uri(r"file://C:\Users\test\repo").is_some());
}

#[test]
fn fp_01_file_db_and_transcript_fingerprints() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("conv.db");
    let wal_path = tmp.path().join("conv.db-wal");

    let missing_fp = DbFingerprint::from_db_path(&db_path);
    assert!(!missing_fp.exists());
    assert_eq!(missing_fp.db, FileStamp::missing());

    std::fs::write(&db_path, b"abc").unwrap();
    let fp1 = DbFingerprint::from_db_path(&db_path);
    assert!(fp1.exists());
    assert_eq!(fp1.db.size, 3);
    assert!(!fp1.wal.exists);

    std::fs::write(&wal_path, b"wal-bytes").unwrap();
    let fp2 = DbFingerprint::from_db_path(&db_path);
    assert!(fp2.wal.exists);
    assert_eq!(fp2.wal.size, 9);
    assert_ne!(fp1, fp2);

    let logs_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();
    std::fs::write(logs_dir.join("transcript_full.jsonl"), b"{}\n").unwrap();
    let tfp = TranscriptFingerprint::from_logs_dir(&logs_dir);
    assert!(tfp.exists());
    assert!(tfp.full.exists);
    assert!(!tfp.compact.exists);
}

#[test]
fn sql_01_readonly_enforced_and_wal_readable() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");

    let rw = Connection::open(&db_path).unwrap();
    rw.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO items (id, name) VALUES (1, 'initial');",
    )
    .unwrap();

    // Verify read-only rejects writes
    let ro = open_readonly(&db_path).unwrap();
    let write_res = ro.execute("INSERT INTO items (id, name) VALUES (2, 'bad')", []);
    assert!(write_res.is_err());

    let val: String = ro
        .query_row("SELECT name FROM items WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(val, "initial");
    drop(ro);

    // Without immutable=1, new WAL commits are immediately visible to a new readonly connection
    rw.execute("INSERT INTO items (id, name) VALUES (2, 'wal-visible')", [])
        .unwrap();
    let val2 = with_readonly_retry(&db_path, |c| {
        Ok(c.query_row("SELECT name FROM items WHERE id = 2", [], |r| {
            r.get::<_, String>(0)
        })?)
    })
    .unwrap();
    assert_eq!(val2, "wal-visible");
}

#[test]
fn sql_02_busy_lock_retries_and_returns_db_busy_or_recovers() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("locked.db");

    let lock_conn = Connection::open(&db_path).unwrap();
    lock_conn
        .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
        .unwrap();

    // Hold an EXCLUSIVE lock on lock_conn so reader hits SQLITE_BUSY across all 3 retries.
    lock_conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let start = Instant::now();
    let err = with_readonly_retry(&db_path, |c| {
        Ok(c.query_row("SELECT x FROM t", [], |r| r.get::<_, i64>(0))?)
    })
    .unwrap_err();
    let elapsed = start.elapsed();

    assert!(matches!(err, CoreError::DbBusy { .. }), "got {err:?}");
    // 150ms busy_timeout * 4 attempts + (30 + 80 + 160)ms backoff >= ~400ms
    assert!(elapsed >= Duration::from_millis(250));

    // Release lock asynchronously mid-retry -> reader succeeds
    let db_path_clone = db_path.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        lock_conn.execute_batch("COMMIT;").unwrap();
    });

    let val = with_readonly_retry(&db_path_clone, |c| {
        Ok(c.query_row("SELECT x FROM t", [], |r| r.get::<_, i64>(0))?)
    })
    .unwrap();
    assert_eq!(val, 1);
    handle.join().unwrap();
}

#[test]
fn sess_01_summaries_inheritance_truncation_mtime_and_brain_supplement() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    std::fs::create_dir_all(app.join("brain")).unwrap();
    std::fs::create_dir_all(app.join("conversations")).unwrap();
    std::fs::write(
        gemini.join("projects.json"),
        r#"{"projects":{"E:\\Work\\alpha":"AlphaProject"}}"#,
    )
    .unwrap();

    let root_id = "11111111-1111-1111-1111-111111111111";
    let child_id = "22222222-2222-2222-2222-222222222222";
    let preview_only_id = "33333333-3333-3333-3333-333333333333";
    let brain_unreg_id = "44444444-4444-4444-4444-444444444444";

    let sum_db = app.join("conversation_summaries.db");
    let conn = Connection::open_with_flags(
        &sum_db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE conversation_summaries (
            conversation_id TEXT PRIMARY KEY,
            title TEXT NOT NULL DEFAULT '',
            preview TEXT NOT NULL DEFAULT '',
            workspace_uris TEXT NOT NULL DEFAULT '',
            project_id TEXT NOT NULL DEFAULT '',
            agent_name TEXT NOT NULL DEFAULT '',
            parent_conversation_id TEXT NOT NULL DEFAULT '',
            nesting_depth INTEGER NOT NULL DEFAULT 0,
            last_modified_time TEXT NOT NULL DEFAULT ''
        );",
    )
    .unwrap();

    conn.execute(
        "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            root_id,
            "Root Session",
            "",
            r#"["file:///E%3A/Work/alpha"]"#,
            "proj-uuid-1",
            "",
            "",
            0,
            "2026-11-14 08:33:44.0033438+00:00"
        ],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            child_id,
            "",
            "",
            "",
            "outside-of-project",
            "research-worker",
            root_id,
            1,
            "2026-11-14 08:34:00+00:00"
        ],
    )
    .unwrap();

    // Japanese 65-char preview -> truncated safely at 60 chars + "..."
    let jp_preview = "あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわをんアイウエオカキクケコサシスセソタチツテト";
    conn.execute(
        "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            preview_only_id,
            "",
            jp_preview,
            "",
            "",
            "",
            "",
            0,
            "2026-11-14 09:00:00+00:00"
        ],
    )
    .unwrap();
    drop(conn);

    // Unregistered brain conversation with a transcript
    let unreg_logs = app
        .join("brain")
        .join(brain_unreg_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&unreg_logs).unwrap();
    std::fs::write(
        unreg_logs.join("transcript.jsonl"),
        "{\"type\":\"USER_INPUT\",\"content\":\"hi\"}\n",
    )
    .unwrap();

    assert_eq!(truncate_chars(jp_preview, 60).chars().count(), 63);
    assert!(parse_datetime_ms("2026-11-14 08:33:44.0033438+00:00") > 0);

    let index = load_session_index(&GeminiPaths::from_gemini_dir(&gemini));
    let root = &index.by_id[root_id];
    assert_eq!(root.display_title, "Root Session");
    assert_eq!(root.project_label, "AlphaProject");

    let child = &index.by_id[child_id];
    assert!(child.is_subagent);
    assert_eq!(child.display_title, "research-worker");
    assert_eq!(child.root_session_id, root_id);
    assert_eq!(child.project_label, "AlphaProject");

    let prev = &index.by_id[preview_only_id];
    assert!(prev.display_title.ends_with("..."));
    assert_eq!(prev.display_title.chars().count(), 63);

    let unreg = &index.by_id[brain_unreg_id];
    assert_eq!(unreg.display_title, "44444444");
    assert!(unreg.has_transcript);
    assert!(unreg.effective_mtime_ms > 0);
}

#[test]
fn ctx_01_transcript_merge_torn_line_utf8_and_bucket_reconcile() {
    // UTF-8 safety on est_text_tokens (multibyte Japanese & emoji > 2000 chars)
    let long_jp = "日本語テスト🚀".repeat(400);
    let est = est_text_tokens(&long_jp);
    assert!(est > 500);

    let tmp = TempDir::new().unwrap();
    let app = tmp.path().join("antigravity");
    let conv_id = "aaaaaaaa-1111-2222-3333-444444444444";
    let logs = app
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();

    let full_lines = [
        r#"{"step_index":0,"type":"USER_INPUT","content":"こんにちは"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","input_tokens":1000,"cache_read_tokens":9000,"output_tokens":500,"content":"了解しました"}"#,
    ]
    .join("\n")
        + "\n";
    std::fs::write(logs.join("transcript_full.jsonl"), full_lines).unwrap();

    // Sleep slightly so compact transcript has newer mtime
    std::thread::sleep(Duration::from_millis(20));
    let compact_lines = [
        r#"{"step_index":0,"type":"USER_INPUT","content":"こんにちは"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","input_tokens":1000,"cache_read_tokens":9000,"output_tokens":500,"content":"了解しました"}"#,
        r#"{"step_index":2,"type":"USER_INPUT","content":"続き"}"#,
        r#"{"step_index":3,"type":"PLANNER_RESPONSE","input_tokens":600,"cache_read_tokens":10000,"output_tokens":300,"content":"はい"}"#,
        r#"{"step_index":4,"type":"USER_INPUT","content":"torn"#, // torn trailing line!
    ]
    .join("\n");
    std::fs::write(logs.join("transcript.jsonl"), compact_lines).unwrap();

    let res = analyze_conversation(&app, conv_id, None, None).unwrap();
    assert!(res.found);
    assert!(res.exact);
    assert_eq!(res.step_count, 4);
    assert_eq!(res.planner_turn_count, 2);
    assert!(res.buckets.customization_breakdown.is_none());
    // exact target = last_prompt (10600) + last_out (300) = 10900
    assert_eq!(res.used_tokens, 10_900);
    assert_eq!(res.buckets.active_total(), 10_900);

    // Direct test of reconcile_active_buckets
    let mut b = ContextBuckets {
        user_messages: 100,
        agent_replies: 200,
        agent_thinking: 300,
        system_prompt: 400,
        ..Default::default()
    };
    reconcile_active_buckets(&mut b, 1500);
    assert_eq!(b.active_total(), 1500);
    reconcile_active_buckets(&mut b, 500);
    assert_eq!(b.active_total(), 500);

    // Torn invalid UTF-8 trailing bytes must not discard valid preceding lines
    let mut torn_utf8_bytes = full_lines_bytes_for_test();
    torn_utf8_bytes.extend_from_slice(&[0xe3, 0x81]); // incomplete UTF-8 sequence
    std::fs::write(logs.join("transcript_full.jsonl"), torn_utf8_bytes).unwrap();
    let res_utf8 = analyze_conversation(&app, conv_id, None, None).unwrap();
    assert!(res_utf8.found);
    assert!(res_utf8.step_count >= 2);
}

fn full_lines_bytes_for_test() -> Vec<u8> {
    [
        r#"{"step_index":0,"type":"USER_INPUT","content":"こんにちは"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","input_tokens":1000,"cache_read_tokens":9000,"output_tokens":500,"content":"了解しました"}"#,
        r#"{"step_index":2,"type":"USER_INPUT","content":"途中で切れた"#,
    ]
    .join("\n")
    .into_bytes()
}

#[test]
fn ctx_02_explicit_and_implicit_compaction_detection() {
    let tmp = TempDir::new().unwrap();
    let app = tmp.path().join("antigravity");
    let conv_id = "bbbbbbbb-1111-2222-3333-444444444444";
    let logs = app
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();

    // Turn 1: 40,000 -> Explicit CHECKPOINT -> Turn 2: 12,000 (saved 28,000)
    // Turn 3: 50,000 -> Implicit drop to 15,000 (drop 35,000 > max(5000, 50000*0.12)=6000)
    // Turn 4: 13,500 (drop 1,500 <= 5000 -> NOT a compaction)
    let lines = [
        r#"{"step_index":0,"type":"USER_INPUT","content":"start"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","input_tokens":10000,"cache_read_tokens":30000,"output_tokens":500,"content":"r1"}"#,
        r#"{"step_index":2,"type":"CHECKPOINT","input_tokens":40500,"cache_read_tokens":0,"output_tokens":1200,"content":"summary"}"#,
        r#"{"step_index":3,"type":"PLANNER_RESPONSE","input_tokens":2000,"cache_read_tokens":10000,"output_tokens":400,"content":"r2"}"#,
        r#"{"step_index":4,"type":"PLANNER_RESPONSE","input_tokens":5000,"cache_read_tokens":45000,"output_tokens":400,"content":"r3"}"#,
        r#"{"step_index":5,"type":"PLANNER_RESPONSE","input_tokens":3000,"cache_read_tokens":12000,"output_tokens":300,"content":"r4"}"#,
        r#"{"step_index":6,"type":"PLANNER_RESPONSE","input_tokens":1500,"cache_read_tokens":12000,"output_tokens":200,"content":"r5"}"#,
    ]
    .join("\n")
        + "\n";
    std::fs::write(logs.join("transcript_full.jsonl"), lines).unwrap();

    let res = analyze_conversation(&app, conv_id, None, None).unwrap();
    assert_eq!(res.compactions.len(), 2);
    assert!(res.compactions[0].is_explicit);
    assert_eq!(res.compactions[0].before_tokens, 40_500);
    assert_eq!(res.compactions[0].after_tokens, Some(12_000));
    assert_eq!(res.compactions[0].saved_tokens, 28_500);

    assert!(!res.compactions[1].is_explicit);
    assert_eq!(res.compactions[1].before_tokens, 50_000);
    assert_eq!(res.compactions[1].after_tokens, Some(15_000));
    assert_eq!(res.compactions[1].saved_tokens, 35_000);
    assert_eq!(res.total_saved_tokens, 63_500);

    // StepSummary uncached_tokens and compacted flags
    assert_eq!(res.step_summaries.len(), 7);
    assert_eq!(res.step_summaries[1].uncached_tokens, 10_000);
    assert_eq!(res.step_summaries[1].cached_tokens, 30_000);
    assert_eq!(res.step_summaries[1].prompt_tokens, 40_000);
    assert!((res.step_summaries[1].cache_hit_percent() - 75.0).abs() < 0.01);
    assert!(!res.step_summaries[1].compacted);
    assert!(res.step_summaries[2].compacted); // explicit CHECKPOINT
    assert!(!res.step_summaries[3].compacted); // post-checkpoint planner turn
    assert!(res.step_summaries[5].compacted); // implicit compaction turn
    assert_eq!(res.step_summaries[5].uncached_tokens, 3_000);
    assert!(!res.step_summaries[6].compacted); // sub-threshold drop
}

#[test]
fn ctx_03_cache_ratios_heavy_tools_top15_and_subagents() {
    let tmp = TempDir::new().unwrap();
    let app = tmp.path().join("antigravity");
    let conv_id = "cccccccc-1111-2222-3333-444444444444";
    let logs = app
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();

    let sub_uuid = "99999999-aaaa-bbbb-cccc-dddddddddddd";
    let mut lines = Vec::new();
    lines.push(r#"{"step_index":0,"type":"USER_INPUT","content":"scan"}"#.to_string());

    // Emit a planner turn with 18 tool calls + 1 invoke_subagent
    let mut tcs = Vec::new();
    for i in 0..18 {
        tcs.push(format!(
            r#"{{"name":"read_file","args":{{"Path":"C:\\repo\\file_{i}.rs"}}}}"#
        ));
    }
    tcs.push(
        r#"{"name":"invoke_subagent","args":{"TypeName":"research","Role":"investigate"}}"#
            .to_string(),
    );

    lines.push(format!(
        r#"{{"step_index":1,"type":"PLANNER_RESPONSE","input_tokens":1000,"cache_read_tokens":3000,"output_tokens":500,"tool_calls":[{}]}}"#,
        tcs.join(",")
    ));

    for i in 0..18 {
        let body = "x".repeat((i + 1) * 50);
        lines.push(format!(
            r#"{{"step_index":{},"type":"GENERIC","content":"{body}"}}"#,
            i + 2
        ));
    }
    lines.push(format!(
        r#"{{"step_index":20,"type":"GENERIC","content":"spawned {sub_uuid}"}}"#
    ));
    lines.push(
        r#"{"step_index":21,"type":"PLANNER_RESPONSE","input_tokens":2000,"cache_read_tokens":18000,"output_tokens":400,"content":"done"}"#
            .to_string(),
    );

    std::fs::write(logs.join("transcript_full.jsonl"), lines.join("\n") + "\n").unwrap();

    let res = analyze_conversation(&app, conv_id, None, None).unwrap();
    // Latest cache: 18000 / 20000 = 90.0%
    assert_eq!(res.cache.cache_hit_ratio, 90.0);
    // Cumulative cache: (3000 + 18000) / (4000 + 20000) = 21000 / 24000 = 87.5%
    assert_eq!(res.cache.session_cache_hit_ratio, 87.5);
    assert_eq!(res.total_tool_calls, 19);
    assert_eq!(res.heavy_tool_calls.len(), 15);
    assert!(res.heavy_tool_calls[0].tokens >= res.heavy_tool_calls[14].tokens);
    assert_eq!(res.spawned_subagents.len(), 1);
    assert_eq!(
        res.spawned_subagents[0].conversation_id.as_deref(),
        Some(sub_uuid)
    );
    assert_eq!(res.step_summaries.len(), 22);
}

#[test]
fn mon_01_poller_fingerprint_skip_commands_and_drop_join() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    let older_id = "dddddddd-1111-2222-3333-444444444444";
    let newer_id = "dddddddd-1111-2222-3333-555555555555";

    let older_logs = app
        .join("brain")
        .join(older_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&older_logs).unwrap();
    let older_transcript = older_logs.join("transcript_full.jsonl");
    std::fs::write(
        &older_transcript,
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"older\"}\n",
    )
    .unwrap();
    // Make older_id strictly older than newer_id regardless of filesystem timestamp granularity.
    let past_time = SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&older_transcript)
        .unwrap()
        .set_modified(past_time)
        .unwrap();

    let newer_logs = app
        .join("brain")
        .join(newer_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&newer_logs).unwrap();
    let newer_transcript = newer_logs.join("transcript_full.jsonl");
    std::fs::write(
        &newer_transcript,
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hello\"}\n",
    )
    .unwrap();

    let notify_count = Arc::new(AtomicUsize::new(0));
    let notify_clone = Arc::clone(&notify_count);
    let mut cfg = MonitorConfig::new(GeminiPaths::from_gemini_dir(&gemini));
    cfg.poll_interval = Duration::from_millis(40);
    cfg.notify = Some(Arc::new(move || {
        notify_clone.fetch_add(1, Ordering::SeqCst);
    }));

    let handle = MonitorHandle::start(cfg);

    // Wait for initial poll -> follow_latest=true selects newer_id
    let deadline = Instant::now() + Duration::from_secs(2);
    while handle.snapshot().last_updated_at.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let snap1 = handle.snapshot();
    assert!(snap1.follow_latest);
    assert_eq!(snap1.selected_id.as_deref(), Some(newer_id));
    assert_eq!(snap1.analysis_count, 1);

    // Let several poll intervals pass without changing files -> analysis_count must remain 1
    std::thread::sleep(Duration::from_millis(150));
    let snap2 = handle.snapshot();
    assert_eq!(snap2.analysis_count, 1);

    // Append a line to newer_id -> fingerprint changes -> analysis_count increments to 2
    std::fs::write(
        &newer_transcript,
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hello\"}\n{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":500,\"cache_read_tokens\":1500,\"output_tokens\":100}\n",
    )
    .unwrap();
    let deadline2 = Instant::now() + Duration::from_secs(2);
    while handle.snapshot().analysis_count < 2 && Instant::now() < deadline2 {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(handle.snapshot().analysis_count, 2);

    // Manually selecting the already-selected ID while follow_latest=true must turn follow_latest=false
    handle.send(MonitorCommand::SelectConversation(Some(
        newer_id.to_string(),
    )));
    let deadline_same = Instant::now() + Duration::from_secs(2);
    while handle.snapshot().follow_latest && Instant::now() < deadline_same {
        std::thread::sleep(Duration::from_millis(10));
    }
    let snap_same = handle.snapshot();
    assert!(!snap_same.follow_latest);
    assert_eq!(snap_same.selected_id.as_deref(), Some(newer_id));

    // Re-enable follow_latest before testing SelectConversation(Some(older_id)) alone
    handle.send(MonitorCommand::SetFollowLatest(true));
    let deadline_re_follow = Instant::now() + Duration::from_secs(2);
    while !handle.snapshot().follow_latest && Instant::now() < deadline_re_follow {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.snapshot().follow_latest);

    // Send SelectConversation(Some(older_id)) ALONE -> follow_latest=false & selected_id=older_id
    handle.send(MonitorCommand::SelectConversation(Some(
        older_id.to_string(),
    )));
    let deadline3 = Instant::now() + Duration::from_secs(2);
    while (handle.snapshot().follow_latest
        || handle.snapshot().selected_id.as_deref() != Some(older_id))
        && Instant::now() < deadline3
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let snap_older = handle.snapshot();
    assert!(!snap_older.follow_latest);
    assert_eq!(snap_older.selected_id.as_deref(), Some(older_id));

    // Update newer_id while follow_latest=false -> manual selection must NOT be overwritten
    let before_update = snap_older.last_updated_at;
    std::fs::write(
        &newer_transcript,
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hello\"}\n{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":500,\"cache_read_tokens\":1500,\"output_tokens\":100}\n{\"step_index\":2,\"type\":\"USER_INPUT\",\"content\":\"newer update\"}\n",
    )
    .unwrap();
    let deadline_pin = Instant::now() + Duration::from_secs(2);
    while handle.snapshot().last_updated_at <= before_update && Instant::now() < deadline_pin {
        std::thread::sleep(Duration::from_millis(10));
    }
    let after_first_poll = handle.snapshot().last_updated_at;
    assert!(after_first_poll > before_update);
    // Wait one more poll cycle as well
    while handle.snapshot().last_updated_at <= after_first_poll && Instant::now() < deadline_pin {
        std::thread::sleep(Duration::from_millis(10));
    }
    let snap_pinned = handle.snapshot();
    assert!(!snap_pinned.follow_latest);
    assert_eq!(snap_pinned.selected_id.as_deref(), Some(older_id));

    // SetFollowLatest(true) switches back to tracking newer_id
    handle.send(MonitorCommand::SetFollowLatest(true));
    let deadline4 = Instant::now() + Duration::from_secs(2);
    while (!handle.snapshot().follow_latest
        || handle.snapshot().selected_id.as_deref() != Some(newer_id))
        && Instant::now() < deadline4
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let snap_resumed = handle.snapshot();
    assert!(snap_resumed.follow_latest);
    assert_eq!(snap_resumed.selected_id.as_deref(), Some(newer_id));

    // Pause, then SelectConversation(Some(older_id)) while paused -> still sets follow_latest=false and selects older_id
    handle.send(MonitorCommand::SetPaused(true));
    let deadline_pause = Instant::now() + Duration::from_secs(2);
    while !handle.snapshot().paused && Instant::now() < deadline_pause {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.snapshot().paused);
    handle.send(MonitorCommand::SelectConversation(Some(
        older_id.to_string(),
    )));
    let deadline_paused_sel = Instant::now() + Duration::from_secs(2);
    while (handle.snapshot().follow_latest
        || handle.snapshot().selected_id.as_deref() != Some(older_id))
        && Instant::now() < deadline_paused_sel
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let snap_paused_sel = handle.snapshot();
    assert!(snap_paused_sel.paused);
    assert!(!snap_paused_sel.follow_latest);
    assert_eq!(snap_paused_sel.selected_id.as_deref(), Some(older_id));

    // RefreshNow while paused still works, Drop cleanly joins
    handle.send(MonitorCommand::RefreshNow);
    std::thread::sleep(Duration::from_millis(50));
    assert!(notify_count.load(Ordering::SeqCst) >= 2);
    drop(handle);
}

#[test]
fn mon_02_db_busy_does_not_freeze_cache_key() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    let conv_id = "eeeeeeee-1111-2222-3333-444444444444";
    let logs = app
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::create_dir_all(app.join("conversations")).unwrap();
    std::fs::write(
        logs.join("transcript_full.jsonl"),
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hello\"}\n",
    )
    .unwrap();

    // Populate conversation DB with claude-opus-4-6 + 320k limit, then hold an exclusive lock
    let db_path = app.join("conversations").join(format!("{conv_id}.db"));
    let lock_conn = Connection::open(&db_path).unwrap();
    lock_conn
        .execute_batch(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
             CREATE TABLE executor_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
        )
        .unwrap();
    let mut step_model = Vec::new();
    encode_varint_field(1, 1026, &mut step_model);
    let mut step_meta = Vec::new();
    encode_bytes_field(24, &step_model, &mut step_meta);
    lock_conn
        .execute(
            "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
            [&step_meta],
        )
        .unwrap();
    let mut cp = Vec::new();
    encode_varint_field(5, 320_000, &mut cp);
    let mut cc = Vec::new();
    encode_bytes_field(2, &cp, &mut cc);
    let mut exec_root = Vec::new();
    encode_bytes_field(10, &cc, &mut exec_root);
    lock_conn
        .execute(
            "INSERT INTO executor_metadata (idx, data) VALUES (1, ?1)",
            [&exec_root],
        )
        .unwrap();

    lock_conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();

    let mut cfg = MonitorConfig::new(GeminiPaths::from_gemini_dir(&gemini));
    cfg.poll_interval = Duration::from_millis(50);
    let handle = MonitorHandle::start(cfg);

    // Wait for first poll (which hits db_busy)
    let deadline = Instant::now() + Duration::from_secs(6);
    while handle.snapshot().last_updated_at.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let snap_busy = handle.snapshot();
    assert!(snap_busy.db_busy);

    // Release exclusive lock without changing DB file mtime/size (ROLLBACK)
    lock_conn.execute_batch("ROLLBACK;").unwrap();
    drop(lock_conn);

    // Monitor must NOT have frozen the cache key while db_busy was true;
    // subsequent poll should re-analyze and resolve the 320k limit and model.
    let deadline2 = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline2 {
        let s = handle.snapshot();
        if !s.db_busy
            && s.analysis
                .as_ref()
                .is_some_and(|a| a.context_limit == 320_000)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let snap_ok = handle.snapshot();
    assert!(!snap_ok.db_busy);
    let analysis = snap_ok.analysis.expect("analysis should exist");
    assert_eq!(analysis.context_limit, 320_000);
    assert_eq!(analysis.model.model_id, "claude-opus-4-6");
}

#[test]
#[ignore = "reads real ~/.gemini/antigravity data on the local machine (read-only smoke check)"]
fn smoke_01_readonly_real_gemini_data() {
    let paths = GeminiPaths::discover().expect("home dir should exist");
    let sum_db = paths
        .gemini_dir
        .join("antigravity")
        .join("conversation_summaries.db");
    let before_stamp = FileStamp::from_path(&sum_db);

    let index = load_session_index(&paths);
    println!(
        "roots={}, total_sessions={}, projects={}, db_busy={}",
        index.roots.len(),
        index.by_id.len(),
        index.projects.len(),
        index.db_busy
    );
    assert!(
        !index.roots.is_empty(),
        "expected at least one session in ~/.gemini"
    );

    let mut analyzed = 0usize;
    for node in index.roots.iter().take(5) {
        let s = &node.summary;
        let res = analyze_conversation(&s.app_data_dir, &s.conversation_id, None, None)
            .expect("analyze_conversation should not error");
        println!(
            "  id={} title={:?} proj={:?} model={} limit={} used={} ({:.1}%) cache={:.1}% exact={} busy={}",
            &s.conversation_id[..8.min(s.conversation_id.len())],
            s.display_title,
            s.project_label,
            res.model.model_label,
            res.context_limit,
            res.used_tokens,
            res.used_percent,
            res.cache.cache_hit_ratio,
            res.exact,
            res.db_busy
        );
        analyzed += 1;
    }
    assert!(analyzed > 0);

    let after_stamp = FileStamp::from_path(&sum_db);
    assert_eq!(
        before_stamp, after_stamp,
        "read-only smoke must not modify existing DB"
    );
}

#[test]
fn model_02_db_only_model_label_and_short_format() {
    let tmp = TempDir::new().unwrap();
    let app = tmp.path().join(".gemini").join("antigravity");
    let conv_dir = app.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    // Missing DB -> (None, false) even if antigravity_state.pbtxt exists
    std::fs::write(
        app.join("antigravity_state.pbtxt"),
        "last_selected_agent_model: MODEL_PLACEHOLDER_M26\n",
    )
    .unwrap();
    let missing_id = "11111111-aaaa-bbbb-cccc-000000000001";
    assert_eq!(
        detect_conversation_model_label_from_db(&app, missing_id),
        (None, false)
    );

    // Populated steps table -> returns formatted label without falling back to pbtxt
    let valid_id = "11111111-aaaa-bbbb-cccc-000000000002";
    let db_path = conv_dir.join(format!("{valid_id}.db"));
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);")
        .unwrap();
    let mut step_model = Vec::new();
    encode_varint_field(1, 1026, &mut step_model); // claude-opus-4-6
    let mut step_meta = Vec::new();
    encode_bytes_field(24, &step_model, &mut step_meta);
    conn.execute(
        "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
        [&step_meta],
    )
    .unwrap();
    drop(conn);

    assert_eq!(
        detect_conversation_model_label_from_db(&app, valid_id),
        (Some("Claude Opus 4.6".to_string()), false)
    );

    // Short label rules
    assert_eq!(
        format_model_short_label("Claude Opus 4.6 (High)"),
        "Claude Opus 4.6 (H)"
    );
    assert_eq!(
        format_model_short_label("Gemini 4 Hydrogen (Medium)"),
        "Gemini 4 Hydrogen (M)"
    );
    assert_eq!(
        format_model_short_label("Gemini 3.8 Flash (Low)"),
        "Gemini 3.8 Flash (L)"
    );
    assert_eq!(
        format_model_short_label("Orchestrator (Carbon)"),
        "Orch (Carbon)"
    );
    assert_eq!(
        format_model_short_label("Gemini Flash Worker"),
        "Flash Worker"
    );
    assert_eq!(format_model_short_label("Gemini 4 Neon"), "Gemini 4 Neon");
    assert_eq!(format_model_short_label("Default model"), "");
}

#[test]
fn mon_03_poller_populates_sidebar_model_labels_with_fingerprint_cache() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    let conv_dir = app.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    let id_a = "aaaaaaaa-0000-0000-0000-000000000001";
    let id_b = "bbbbbbbb-0000-0000-0000-000000000002";
    for (id, text) in [
        (
            id_a,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"a\"}\n",
        ),
        (
            id_b,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"b\"}\n",
        ),
    ] {
        let logs = app
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("transcript_full.jsonl"), text).unwrap();
    }

    // id_a has DB (claude-opus-4-6), id_b has DB locked initially
    let write_model_db = |db_path: &std::path::Path, model_id: u64| {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS steps (idx INTEGER PRIMARY KEY, metadata BLOB);",
        )
        .unwrap();
        let mut step_model = Vec::new();
        encode_varint_field(1, model_id, &mut step_model);
        let mut step_meta = Vec::new();
        encode_bytes_field(24, &step_model, &mut step_meta);
        conn.execute(
            "INSERT OR REPLACE INTO steps (idx, metadata) VALUES (1, ?1)",
            [&step_meta],
        )
        .unwrap();
    };

    let db_a = conv_dir.join(format!("{id_a}.db"));
    write_model_db(&db_a, 1026); // Claude Opus 4.6

    let db_b = conv_dir.join(format!("{id_b}.db"));
    let lock_b = Connection::open(&db_b).unwrap();
    lock_b
        .execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);")
        .unwrap();
    let mut step_model_b = Vec::new();
    encode_varint_field(1, 1525, &mut step_model_b); // Gemini 4 Hydrogen (High)
    let mut step_meta_b = Vec::new();
    encode_bytes_field(24, &step_model_b, &mut step_meta_b);
    lock_b
        .execute(
            "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
            [&step_meta_b],
        )
        .unwrap();
    lock_b.execute_batch("BEGIN EXCLUSIVE;").unwrap();

    let mut cfg = MonitorConfig::new(GeminiPaths::from_gemini_dir(&gemini));
    cfg.poll_interval = Duration::from_millis(40);
    let handle = MonitorHandle::start(cfg);

    let deadline1 = Instant::now() + Duration::from_secs(6);
    while handle
        .snapshot()
        .index
        .by_id
        .get(id_a)
        .map(|s| s.model_label.as_str())
        != Some("Claude Opus 4.6")
        && Instant::now() < deadline1
    {
        std::thread::sleep(Duration::from_millis(15));
    }
    let snap1 = handle.snapshot();
    assert_eq!(
        snap1.index.by_id.get(id_a).map(|s| s.model_label.as_str()),
        Some("Claude Opus 4.6")
    );
    // Locked db_b must not freeze empty cache permanently
    assert_eq!(
        snap1.index.by_id.get(id_b).map(|s| s.model_label.as_str()),
        Some("")
    );

    // Release exclusive lock on db_b without altering its file mtime -> next poll resolves db_b
    lock_b.execute_batch("COMMIT;").unwrap();
    drop(lock_b);

    let deadline2 = Instant::now() + Duration::from_secs(3);
    while handle
        .snapshot()
        .index
        .by_id
        .get(id_b)
        .map(|s| s.model_label.as_str())
        != Some("Gemini 4 Hydrogen (High)")
        && Instant::now() < deadline2
    {
        std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(
        handle
            .snapshot()
            .index
            .by_id
            .get(id_b)
            .map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)")
    );

    // Update db_a to another model -> fingerprint changes and sidebar model_label updates
    std::thread::sleep(Duration::from_millis(30));
    write_model_db(&db_a, 1035); // Claude Sonnet 4.6
    let deadline3 = Instant::now() + Duration::from_secs(3);
    while handle
        .snapshot()
        .index
        .by_id
        .get(id_a)
        .map(|s| s.model_label.as_str())
        != Some("Claude Sonnet 4.6")
        && Instant::now() < deadline3
    {
        std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(
        handle
            .snapshot()
            .index
            .by_id
            .get(id_a)
            .map(|s| s.model_label.as_str()),
        Some("Claude Sonnet 4.6")
    );

    drop(handle);
}

#[test]
fn mon_04_pbtxt_fallback_isolated_from_sidebar_model_cache() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    let conv_dir = app.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    // Global pbtxt fallback maps MODEL_PLACEHOLDER_M26 -> "Claude Opus 4.6"
    std::fs::write(
        app.join("antigravity_state.pbtxt"),
        "last_selected_agent_model: MODEL_PLACEHOLDER_M26\n",
    )
    .unwrap();

    let id_x = "aaaaaaaa-0000-0000-0000-000000000010";
    let id_y = "bbbbbbbb-0000-0000-0000-000000000020";
    for (id, text) in [
        (
            id_x,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"session x\"}\n",
        ),
        (
            id_y,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"session y\"}\n",
        ),
    ] {
        let logs = app
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("transcript_full.jsonl"), text).unwrap();
    }

    // Session X has a DB with no model metadata; Session Y has Gemini 4 Hydrogen (High) in DB.
    let db_x = conv_dir.join(format!("{id_x}.db"));
    {
        let conn_x = Connection::open(&db_x).unwrap();
        conn_x
            .execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);")
            .unwrap();
    }

    let db_y = conv_dir.join(format!("{id_y}.db"));
    {
        let conn_y = Connection::open(&db_y).unwrap();
        conn_y
            .execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);")
            .unwrap();
        let mut step_model_y = Vec::new();
        encode_varint_field(1, 1525, &mut step_model_y); // Gemini 4 Hydrogen (High)
        let mut step_meta_y = Vec::new();
        encode_bytes_field(24, &step_model_y, &mut step_meta_y);
        conn_y
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [&step_meta_y],
            )
            .unwrap();
    }

    let mut cfg = MonitorConfig::new(GeminiPaths::from_gemini_dir(&gemini));
    cfg.poll_interval = Duration::from_millis(60);
    cfg.follow_latest = false;
    let handle = MonitorHandle::start(cfg);

    // (a) Select session X without DB model: right pane uses pbtxt fallback, sidebar stays "".
    handle.send(MonitorCommand::SelectConversation(Some(id_x.to_string())));
    let deadline_x = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline_x {
        let snap = handle.snapshot();
        if snap.selected_id.as_deref() == Some(id_x)
            && snap
                .analysis
                .as_ref()
                .is_some_and(|a| a.conversation_id == id_x)
            && snap
                .index
                .by_id
                .get(id_y)
                .is_some_and(|s| s.model_label == "Gemini 4 Hydrogen (High)")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let snap_x = handle.snapshot();
    assert_eq!(
        snap_x
            .analysis
            .as_ref()
            .map(|a| a.model.model_label.as_str()),
        Some("Claude Opus 4.6"),
        "right pane should still use pbtxt fallback for session X"
    );
    assert_eq!(
        snap_x.index.by_id.get(id_x).map(|s| s.model_label.as_str()),
        Some(""),
        "session X without DB model must keep sidebar model_label empty while selected"
    );
    assert_eq!(
        snap_x.index.by_id.get(id_y).map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)")
    );

    // Switch to session Y: X keeps "", Y gets its DB model label.
    handle.send(MonitorCommand::SelectConversation(Some(id_y.to_string())));
    let deadline_y = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline_y {
        let snap = handle.snapshot();
        if snap.selected_id.as_deref() == Some(id_y)
            && snap
                .analysis
                .as_ref()
                .is_some_and(|a| a.conversation_id == id_y)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let snap_y = handle.snapshot();
    assert_eq!(
        snap_y
            .analysis
            .as_ref()
            .map(|a| a.model.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)")
    );
    assert_eq!(
        snap_y.index.by_id.get(id_x).map(|s| s.model_label.as_str()),
        Some(""),
        "session X must retain empty sidebar model_label after switching to session Y"
    );
    assert_eq!(
        snap_y.index.by_id.get(id_y).map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)")
    );

    // (b) Switch away back to session X: Y retains its DB-derived model label in sidebar.
    handle.send(MonitorCommand::SelectConversation(Some(id_x.to_string())));
    let deadline_back_x = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline_back_x {
        let snap = handle.snapshot();
        if snap.selected_id.as_deref() == Some(id_x)
            && snap
                .analysis
                .as_ref()
                .is_some_and(|a| a.conversation_id == id_x)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let snap_back_x = handle.snapshot();
    assert_eq!(
        snap_back_x
            .index
            .by_id
            .get(id_y)
            .map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)"),
        "session Y must retain its DB-derived sidebar label after switching away"
    );
    assert_eq!(
        snap_back_x
            .index
            .by_id
            .get(id_x)
            .map(|s| s.model_label.as_str()),
        Some("")
    );

    // (c) Temporary exclusive lock on Y's DB while Y is selected must not wipe Y's cached label.
    let lock_y = Connection::open(&db_y).unwrap();
    lock_y.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    handle.send(MonitorCommand::SelectConversation(Some(id_y.to_string())));
    let deadline_locked = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline_locked {
        let snap = handle.snapshot();
        if snap.selected_id.as_deref() == Some(id_y)
            && snap
                .analysis
                .as_ref()
                .is_some_and(|a| a.conversation_id == id_y && a.db_busy)
            && snap.db_busy
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snap_locked = handle.snapshot();
    assert!(
        snap_locked.db_busy,
        "expected db_busy while Y's DB is exclusively locked"
    );
    assert_eq!(
        snap_locked
            .index
            .by_id
            .get(id_y)
            .map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)"),
        "exclusive lock on Y's DB must not wipe or overwrite Y's cached DB-derived sidebar label"
    );

    lock_y.execute_batch("COMMIT;").unwrap();
    drop(lock_y);

    let deadline_unlocked = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline_unlocked {
        let snap = handle.snapshot();
        if !snap.db_busy
            && snap
                .analysis
                .as_ref()
                .is_some_and(|a| a.conversation_id == id_y && !a.db_busy)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snap_unlocked = handle.snapshot();
    assert!(!snap_unlocked.db_busy);
    assert_eq!(
        snap_unlocked
            .index
            .by_id
            .get(id_y)
            .map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)")
    );

    drop(handle);
}

#[test]
fn mon_05_backfill_avoids_index_reloads_and_stops_on_busy_session() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    let conv_dir = app.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    let base_mtime = SystemTime::now() - Duration::from_secs(600);
    let write_session = |id: &str, model_id: u64, mtime: SystemTime| -> std::path::PathBuf {
        let logs = app
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let transcript_path = logs.join("transcript_full.jsonl");
        std::fs::write(
            &transcript_path,
            format!("{{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"{id}\"}}\n"),
        )
        .unwrap();

        let db_path = conv_dir.join(format!("{id}.db"));
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);")
                .unwrap();
            let mut step_model = Vec::new();
            encode_varint_field(1, model_id, &mut step_model);
            let mut step_meta = Vec::new();
            encode_bytes_field(24, &step_model, &mut step_meta);
            conn.execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [&step_meta],
            )
            .unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&transcript_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&db_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        db_path
    };

    // 79 background unlocked sessions + 1 locked session (roots[1]) + 1 selected unlocked session (roots[0])
    // = 81 sessions total. Tick 1 scans locked (1) + 39 unlocked; Tick 2 skips locked via busy_this_poll
    // and scans the remaining 40 unlocked sessions (exact budget saturation).
    let mut unlocked_ids = Vec::with_capacity(80);
    for i in 1..=79usize {
        let id = format!("11111111-0000-0000-0000-{i:012}");
        write_session(&id, 1026, base_mtime + Duration::from_secs(i as u64)); // Claude Opus 4.6
        unlocked_ids.push(id);
    }

    let locked_id = "00000000-0000-0000-0000-000000000001";
    let locked_db_path = write_session(locked_id, 1525, base_mtime + Duration::from_secs(200));

    let selected_id = "00000000-0000-0000-0000-000000000000";
    write_session(selected_id, 1026, base_mtime + Duration::from_secs(300));
    unlocked_ids.push(selected_id.to_string());

    let lock_conn = Connection::open(&locked_db_path).unwrap();
    lock_conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();

    let notify_count = Arc::new(AtomicUsize::new(0));
    let notify_clone = Arc::clone(&notify_count);
    let mut cfg = MonitorConfig::new(GeminiPaths::from_gemini_dir(&gemini));
    cfg.poll_interval = Duration::from_secs(5);
    cfg.notify = Some(Arc::new(move || {
        notify_clone.fetch_add(1, Ordering::SeqCst);
    }));
    let handle = MonitorHandle::start(cfg);

    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let snap = handle.snapshot();
        let all_unlocked_done = unlocked_ids.iter().all(|id| {
            snap.index
                .by_id
                .get(id)
                .is_some_and(|s| s.model_label == "Claude Opus 4.6")
        });
        if all_unlocked_done {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }

    let notify_deadline = Instant::now() + Duration::from_secs(1);
    while notify_count.load(Ordering::SeqCst) < 2 && Instant::now() < notify_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(20));

    let snap_done = handle.snapshot();
    assert_eq!(snap_done.selected_id.as_deref(), Some(selected_id));
    assert_eq!(
        snap_done.index.roots[0].summary.conversation_id.as_str(),
        selected_id
    );
    assert_eq!(
        snap_done.index.roots[1].summary.conversation_id.as_str(),
        locked_id,
        "locked session must be roots[1] so Tick 1 guarantees scanning it"
    );
    for id in &unlocked_ids {
        assert_eq!(
            snap_done
                .index
                .by_id
                .get(id)
                .map(|s| s.model_label.as_str()),
            Some("Claude Opus 4.6"),
            "session {id} should finish model backfill"
        );
    }
    assert_eq!(
        snap_done
            .index
            .by_id
            .get(locked_id)
            .map(|s| s.model_label.as_str()),
        Some(""),
        "exclusively locked session should stay empty without blocking backfill completion"
    );
    assert_eq!(
        handle.index_reload_count(),
        1,
        "backfill ticks must not reload the session index"
    );
    assert_eq!(
        handle.model_scan_count(),
        80,
        "locked session must be scanned once on Tick 1 and skipped on Tick 2 (1 locked + 79 background unlocked = 80)"
    );
    assert_eq!(
        notify_count.load(Ordering::SeqCst),
        2,
        "exact 2 ticks (1 initial poll + 1 backfill tick) should complete all 80 unlocked sessions"
    );

    // Confirm backfill stopped spinning at 50ms once only the locked session remained.
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(
        notify_count.load(Ordering::SeqCst),
        2,
        "backfill must stop spinning at 50ms once non-locked sessions complete"
    );
    assert_eq!(handle.model_scan_count(), 80);
    assert_eq!(handle.index_reload_count(), 1);

    drop(handle);
    lock_conn.execute_batch("COMMIT;").unwrap();
    drop(lock_conn);
}

#[test]
fn mon_06_backfill_keeps_fresh_selected_model_when_db_changes() {
    let tmp = TempDir::new().unwrap();
    let gemini = tmp.path().join(".gemini");
    let app = gemini.join("antigravity");
    let conv_dir = app.join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    let base_mtime = SystemTime::now() - Duration::from_secs(600);
    let write_session = |id: &str, model_id: u64, mtime: SystemTime| -> std::path::PathBuf {
        let logs = app
            .join("brain")
            .join(id)
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let transcript_path = logs.join("transcript_full.jsonl");
        std::fs::write(
            &transcript_path,
            format!("{{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"{id}\"}}\n"),
        )
        .unwrap();

        let db_path = conv_dir.join(format!("{id}.db"));
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);")
                .unwrap();
            let mut step_model = Vec::new();
            encode_varint_field(1, model_id, &mut step_model);
            let mut step_meta = Vec::new();
            encode_bytes_field(24, &step_model, &mut step_meta);
            conn.execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [&step_meta],
            )
            .unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&transcript_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&db_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        db_path
    };

    // 41 background unlocked sessions (> MAX_MODEL_SCANS_PER_TICK = 40) + 1 newest selected session
    // so Tick 1 schedules automatic 50ms backfill in run_poller.
    let mut bg_ids = Vec::with_capacity(41);
    for i in 1..=41usize {
        let id = format!("22222222-0000-0000-0000-{i:012}");
        write_session(&id, 1026, base_mtime + Duration::from_secs(i as u64)); // Claude Opus 4.6
        bg_ids.push(id);
    }

    let sel_id = "00000000-0000-0000-0000-000000000000";
    let sel_db_path = write_session(sel_id, 1026, base_mtime + Duration::from_secs(300));

    let notify_count = Arc::new(AtomicUsize::new(0));
    let mutated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let notify_clone = Arc::clone(&notify_count);
    let mutated_clone = Arc::clone(&mutated);
    let sel_db_clone = sel_db_path.clone();
    let updated_mtime = base_mtime + Duration::from_secs(400);

    let mut cfg = MonitorConfig::new(GeminiPaths::from_gemini_dir(&gemini));
    cfg.poll_interval = Duration::from_secs(10);
    cfg.notify = Some(Arc::new(move || {
        let n = notify_clone.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            // Mutate the selected session's DB right after Tick 1 publishes the initial snapshot
            // and before run_poller enters the 50ms backfill tick.
            {
                let conn = Connection::open(&sel_db_clone).unwrap();
                let mut step_model = Vec::new();
                encode_varint_field(1, 1525, &mut step_model); // Gemini 4 Hydrogen (High)
                let mut step_meta = Vec::new();
                encode_bytes_field(24, &step_model, &mut step_meta);
                conn.execute("UPDATE steps SET metadata = ?1 WHERE idx = 1", [&step_meta])
                    .unwrap();
                conn.execute(
                    "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                    [&step_meta],
                )
                .unwrap();
            }
            std::fs::File::options()
                .write(true)
                .open(&sel_db_clone)
                .unwrap()
                .set_modified(updated_mtime)
                .unwrap();
            mutated_clone.store(true, Ordering::SeqCst);
        }
    }));
    let handle = MonitorHandle::start(cfg);

    let deadline = Instant::now() + Duration::from_secs(6);
    while notify_count.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(30));

    assert!(mutated.load(Ordering::SeqCst));
    assert_eq!(
        handle.index_reload_count(),
        1,
        "Tick 2 must run via the 50ms backfill path without reloading the session index"
    );
    let snap = handle.snapshot();
    assert_eq!(snap.selected_id.as_deref(), Some(sel_id));
    assert_eq!(
        snap.analysis.as_ref().map(|a| a.model.model_label.as_str()),
        Some("Claude Opus 4.6"),
        "cached_analysis remains stale during backfill tick"
    );
    assert_eq!(
        snap.index.by_id.get(sel_id).map(|s| s.model_label.as_str()),
        Some("Gemini 4 Hydrogen (High)"),
        "backfill must keep fresh DB model label instead of overwriting with stale cached_analysis"
    );
    assert_eq!(
        snap.index.roots[0].summary.model_label.as_str(),
        "Gemini 4 Hydrogen (High)"
    );
    for id in &bg_ids {
        assert_eq!(
            snap.index.by_id.get(id).map(|s| s.model_label.as_str()),
            Some("Claude Opus 4.6")
        );
    }
    assert_eq!(
        handle.model_scan_count(),
        42,
        "Tick 1 scans 40 background sessions; Tick 2 scans updated selected DB + 1 remaining background session"
    );
    assert_eq!(notify_count.load(Ordering::SeqCst), 2);

    drop(handle);
}
