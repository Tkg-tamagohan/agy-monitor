use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::fingerprint::wal_path_for;
use crate::proto::{decode_fields, proto_bytes, proto_nested, proto_num, proto_text};
use crate::sqlite::{with_readonly_retry, with_readonly_retry_schedule};

pub const DEFAULT_CONTEXT_LIMIT: u64 = 1_048_576;

pub fn lookup_model_id(id: u64) -> Option<&'static str> {
    match id {
        246 => Some("gemini-2.5-pro"),
        312 => Some("gemini-2.5-flash"),
        313 => Some("gemini-2.5-flash-thinking"),
        329 => Some("gemini-2.5-flash-thinking"),
        330 => Some("gemini-2.5-flash-lite"),
        281 | 282 => Some("claude-sonnet-4"),
        290 | 291 => Some("claude-opus-4"),
        333 | 334 => Some("claude-sonnet-4-5"),
        340 | 341 => Some("claude-haiku-4-5"),
        342 => Some("gpt-oss-120b-medium"),
        1016 | 1036 | 1037 => Some("gemini-3.1-pro"),
        1018 | 1047 | 1084 => Some("gemini-3-flash-preview"),
        1020 => Some("gemini-3.5-flash-medium"),
        1021 => Some("gemini-3.1-flash-image"),
        1026 => Some("claude-opus-4-6"),
        1035 => Some("claude-sonnet-4-6"),
        1050 => Some("gemini-3.1-flash-lite"),
        1071 => Some("gemini-3.6-flash-high"),
        1072 => Some("gemini-3.6-flash-medium"),
        1073 => Some("gemini-3.6-flash-low"),
        1132 | 1133 => Some("gemini-3.5-flash-high"),
        1187 => Some("gemini-3.5-flash-extra-low"),
        1196 | 1266 => Some("gemini-3.6-flash"),
        1298 => Some("gemini-3.7-flash-high"),
        1299 => Some("gemini-3.7-flash-medium"),
        1300 => Some("gemini-3.7-flash-low"),
        1301 => Some("gemini-3.7-flash-tiered"),
        1318 => Some("gemini-3.8-flash-high"),
        1319 => Some("gemini-3.8-flash-medium"),
        1320 => Some("gemini-3.8-flash-low"),
        1322 => Some("gemini-3.8-flash-tiered"),
        1400 => Some("claude-opus-5-5-low"),
        1401 => Some("claude-opus-5-5-medium"),
        1402 => Some("claude-opus-5-5-high"),
        1403 => Some("claude-sonnet-5-5-low"),
        1404 => Some("claude-sonnet-5-5-medium"),
        1405 => Some("claude-sonnet-5-5-high"),
        1417 => Some("gemini-4-argon-high"),
        1418 => Some("gemini-4-argon-medium"),
        1419 => Some("gemini-4-argon-low"),
        1427 => Some("gemini-4-carbon-high"),
        1437 => Some("gemini-4-carbon-medium"),
        1438 => Some("gemini-4-carbon-low"),
        1525 => Some("gemini-4-hydrogen-high"),
        1526 => Some("gemini-4-hydrogen-medium"),
        1527 => Some("gemini-4-hydrogen-low"),
        1528 => Some("gemini-4-hydrogen-xhigh"),
        1667 => Some("gemini-pro-orchestrator"),
        1669 => Some("gemini-flash-worker-agy"),
        1681 => Some("gemini-pro-orchestrator-argon"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub model_label: String,
    pub context_limit: u64,
    pub model_max_tokens: u64,
    pub limit_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDetectionResult {
    pub info: ModelInfo,
    pub db_busy: bool,
    pub model_from_db: bool,
}

pub fn normalize_model_slug(raw_name: &str) -> String {
    let trimmed = raw_name.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return String::new();
    }

    // Replace trailing ` (effort)` with `-$1`
    let mut s = if trimmed.ends_with(')')
        && let Some(open_idx) = trimmed.rfind('(')
    {
        let inner = &trimmed[open_idx + 1..trimmed.len() - 1];
        if !inner.contains('(') && !inner.contains(')') {
            let prefix = trimmed[..open_idx].trim_end();
            format!("{prefix}-{inner}")
        } else {
            trimmed
        }
    } else {
        trimmed
    };

    // Collapse whitespace runs to `-`
    s = s.split_whitespace().collect::<Vec<_>>().join("-");

    // If starts with `claude-`, rewrite `claude-<ver>-<tier>` -> `claude-<tier>-<ver>` and `.` -> `-`
    if s.starts_with("claude-") {
        if let Some(rest) = s.strip_prefix("claude-") {
            let parts: Vec<&str> = rest.splitn(3, '-').collect();
            if parts.len() >= 2
                && is_version_token(parts[0])
                && matches!(parts[1], "sonnet" | "opus" | "haiku")
            {
                let mut rebuilt = format!("claude-{}-{}", parts[1], parts[0]);
                if parts.len() == 3 {
                    rebuilt.push('-');
                    rebuilt.push_str(parts[2]);
                }
                s = rebuilt;
            }
        }
        s = s.replace('.', "-");
    }

    s
}

fn is_version_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    let mut parts = tok.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(minor) => {
            !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()) && parts.next().is_none()
        }
    }
}

pub fn is_concrete_model_slug(model: &str) -> bool {
    if model.is_empty() {
        return false;
    }
    let matches_family = model == "gemini-pro-orchestrator"
        || model == "gemini-pro-orchestrator-argon"
        || model == "gemini-flash-worker-agy"
        || starts_with_digit_after(model, "gemini-")
        || starts_with_digit_after(model, "claude-sonnet-")
        || starts_with_digit_after(model, "claude-opus-")
        || starts_with_digit_after(model, "claude-haiku-")
        || model.starts_with("gpt-");

    if !matches_family {
        return false;
    }

    let excluded = matches!(
        model,
        "gemini-3-flash-agent"
            | "gemini-3-flash-agent-a"
            | "gemini-3-flash-agent-b"
            | "gemini-3-flash-a"
            | "gemini-3-flash-b"
            | "gemini-4-argon-b"
            | "gemini-4-argon-f"
            | "gemini-3.8-flash-n"
    );

    !excluded
}

fn starts_with_digit_after(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .and_then(|rest| rest.bytes().next())
        .is_some_and(|b| b.is_ascii_digit())
}

pub fn format_model_label(slug: &str) -> String {
    let id = normalize_model_slug(slug);
    if id.is_empty() {
        return "Default model".to_string();
    }
    if id == "gemini-pro-orchestrator" {
        return "Orchestrator (Carbon)".to_string();
    }
    if id == "gemini-pro-orchestrator-argon" {
        return "Orchestrator (Argon)".to_string();
    }
    if id == "gemini-flash-worker-agy" {
        return "Gemini Flash Worker".to_string();
    }

    let suffixes = [
        "-extra-low",
        "-low",
        "-medium",
        "-high",
        "-xhigh",
        "-tiered",
    ];
    let (base, effort) = suffixes
        .iter()
        .find_map(|sfx| {
            id.strip_suffix(sfx)
                .map(|b| (b, sfx[1..].replace('-', " ")))
        })
        .unwrap_or((id.as_str(), String::new()));

    let label = if let Some(rest) = base.strip_prefix("claude-")
        && let Some((tier, ver_str)) = parse_claude_base(rest)
    {
        format!("Claude {} {ver_str}", title_word(tier))
    } else if let Some(rest) = base.strip_prefix("gpt-oss-") {
        format!("GPT-OSS {}", rest.to_ascii_uppercase())
    } else {
        base.split('-')
            .map(|part| {
                if part.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
                    part.to_string()
                } else {
                    title_word(part)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };

    if effort.is_empty() {
        label
    } else {
        let eff_title = effort
            .split_whitespace()
            .map(title_word)
            .collect::<Vec<_>>()
            .join(" ");
        format!("{label} ({eff_title})")
    }
}

fn parse_claude_base(rest: &str) -> Option<(&str, String)> {
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let tier = parts[0];
    if !matches!(tier, "opus" | "sonnet" | "haiku") {
        return None;
    }
    if parts[1].is_empty() || !parts[1].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if parts.len() == 3 {
        if parts[2].is_empty() || !parts[2].bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some((tier, format!("{}.{}", parts[1], parts[2])))
    } else {
        Some((tier, parts[1].to_string()))
    }
}

fn title_word(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out = c.to_uppercase().to_string();
            out.push_str(chars.as_str());
            out
        }
    }
}

pub fn format_model_short_label(label: &str) -> String {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed == "Default model" {
        return String::new();
    }
    let mut s = match trimmed {
        "Orchestrator (Carbon)" => return "Orch (Carbon)".to_string(),
        "Orchestrator (Argon)" => return "Orch (Argon)".to_string(),
        "Gemini Flash Worker" => return "Flash Worker".to_string(),
        _ => trimmed.to_string(),
    };
    for (full, short) in [
        (" (Extra Low)", " (XL)"),
        (" (Xhigh)", " (XH)"),
        (" (High)", " (H)"),
        (" (Medium)", " (M)"),
        (" (Low)", " (L)"),
        (" (Tiered)", " (T)"),
    ] {
        if let Some(prefix) = s.strip_suffix(full) {
            s = format!("{prefix}{short}");
            break;
        }
    }
    s
}

pub fn map_placeholder_enum_to_slug(enum_name: &str) -> String {
    match enum_name.trim() {
        "" => String::new(),
        "MODEL_OPENAI_GPT_OSS_120B_MEDIUM" => "gpt-oss-120b-medium".to_string(),
        "MODEL_GOOGLE_GEMINI_2_5_FLASH" => "gemini-2.5-flash".to_string(),
        "MODEL_GOOGLE_GEMINI_2_5_PRO" => "gemini-2.5-pro".to_string(),
        "MODEL_CLAUDE_4_5_HAIKU" => "claude-haiku-4-5".to_string(),
        other => {
            if let Some(num_str) = other.strip_prefix("MODEL_PLACEHOLDER_M")
                && !num_str.is_empty()
                && num_str.bytes().all(|b| b.is_ascii_digit())
                && let Ok(n) = num_str.parse::<u64>()
                && let Some(slug) = lookup_model_id(1000 + n)
            {
                return slug.to_string();
            }
            String::new()
        }
    }
}

pub fn resolve_model_context_limit(raw_model: &str, agy_max_token_limit: u64) -> ModelInfo {
    let model_id = normalize_model_slug(raw_model);
    let model_max_tokens = if model_id.is_empty() {
        DEFAULT_CONTEXT_LIMIT
    } else if model_id.starts_with("gpt-oss-120b") {
        131_072
    } else if matches_prefix_boundary(&model_id, "claude-sonnet-4-5")
        || matches_prefix_boundary(&model_id, "claude-opus-4-5")
        || matches_prefix_boundary(&model_id, "claude-haiku-4-5")
        || matches_claude_4_or_4_2(&model_id)
    {
        200_000
    } else if matches_prefix_boundary(&model_id, "claude-opus-5")
        || matches_prefix_boundary(&model_id, "claude-sonnet-5")
        || matches_prefix_boundary(&model_id, "claude-haiku-5")
        || matches_prefix_boundary(&model_id, "claude-opus-4-6")
        || matches_prefix_boundary(&model_id, "claude-opus-4-7")
        || matches_prefix_boundary(&model_id, "claude-opus-4-8")
        || matches_prefix_boundary(&model_id, "claude-sonnet-4-6")
        || matches_prefix_boundary(&model_id, "claude-sonnet-4-7")
        || matches_prefix_boundary(&model_id, "claude-sonnet-4-8")
    {
        1_000_000
    } else if model_id.starts_with("gemini-3.1-flash-image") {
        131_072
    } else if model_id.starts_with("gemini-3-pro-image") {
        65_536
    } else if model_id.starts_with("gemini-2.5-flash-image") {
        32_768
    } else {
        DEFAULT_CONTEXT_LIMIT
    };

    let agy_default = if model_id.contains("orchestrator") {
        512_000
    } else {
        model_max_tokens.min(256_000)
    };

    let context_limit = if agy_max_token_limit > 0 {
        agy_max_token_limit
    } else {
        agy_default
    };

    ModelInfo {
        model_label: format_model_label(&model_id),
        model_id,
        context_limit,
        model_max_tokens,
        limit_source: if agy_max_token_limit > 0 {
            "antigravity".to_string()
        } else {
            "antigravity-default".to_string()
        },
    }
}

fn matches_prefix_boundary(s: &str, prefix: &str) -> bool {
    s == prefix
        || s.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn matches_claude_4_or_4_2(s: &str) -> bool {
    for base in ["claude-sonnet-4", "claude-opus-4"] {
        if s == base || s.starts_with(&format!("{base}-2")) {
            return true;
        }
    }
    false
}

pub fn extract_entry_model_slug(bytes: &[u8], is_step: bool) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let root = decode_fields(bytes);
    if !is_step && proto_bytes(&root, 1).is_none() {
        return String::new();
    }
    let data = if is_step {
        root
    } else {
        proto_nested(&root, 1)
    };
    let model_msg = if is_step {
        proto_nested(&data, 24)
    } else {
        data.clone()
    };

    let primary_usage_id = proto_bytes(&data, if is_step { 9 } else { 4 })
        .map(|b| proto_num(&decode_fields(b), 1))
        .unwrap_or(0);

    let candidates: Vec<String> = [
        proto_text(&model_msg, if is_step { 12 } else { 19 }),
        proto_text(&model_msg, if is_step { 8 } else { 21 }),
    ]
    .into_iter()
    .map(|raw| normalize_model_slug(&raw))
    .filter(|s| !s.is_empty())
    .collect();

    if let Some(concrete) = candidates.iter().find(|c| is_concrete_model_slug(c)) {
        return concrete.clone();
    }

    let model_id = {
        let id = proto_num(&model_msg, if is_step { 1 } else { 3 });
        if id > 0 { id } else { primary_usage_id }
    };
    if model_id > 0
        && let Some(mapped) = lookup_model_id(model_id)
    {
        return mapped.to_string();
    }

    candidates
        .into_iter()
        .find(|n| !n.starts_with("model_") && !n.starts_with("model-"))
        .unwrap_or_default()
}

fn read_db_model_slugs(
    db_path: &Path,
    steps_limit: usize,
    gen_limit: usize,
    read_executor: bool,
    retry_on_busy: bool,
) -> (Vec<String>, u64, bool) {
    let mut found_slugs: Vec<String> = Vec::new();
    let mut agy_max_token_limit: u64 = 0;
    let mut db_busy = false;

    let db_ready = std::fs::metadata(db_path)
        .map(|m| m.is_file() && (m.len() > 0 || wal_path_for(db_path).exists()))
        .unwrap_or(false);

    if db_ready {
        let query_fn = |conn: &rusqlite::Connection| {
            found_slugs.clear();
            agy_max_token_limit = 0;

            let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
            let tables: std::collections::HashSet<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<std::collections::HashSet<String>, rusqlite::Error>>(
                )?;

            if steps_limit > 0 && tables.contains("steps") {
                let sql = format!(
                    "SELECT metadata FROM steps WHERE metadata IS NOT NULL ORDER BY idx DESC LIMIT {steps_limit}"
                );
                let mut s_stmt = conn.prepare(&sql)?;
                let mut rows = s_stmt.query([])?;
                while let Some(row) = rows.next()? {
                    if let Ok(bytes) = row.get::<_, Vec<u8>>(0) {
                        let slug = extract_entry_model_slug(&bytes, true);
                        if !slug.is_empty() {
                            found_slugs.push(slug);
                        }
                    }
                }
            }

            if gen_limit > 0 && tables.contains("gen_metadata") {
                let sql =
                    format!("SELECT data FROM gen_metadata ORDER BY rowid DESC LIMIT {gen_limit}");
                let mut g_stmt = conn.prepare(&sql)?;
                let mut rows = g_stmt.query([])?;
                while let Some(row) = rows.next()? {
                    if let Ok(bytes) = row.get::<_, Vec<u8>>(0) {
                        let slug = extract_entry_model_slug(&bytes, false);
                        if !slug.is_empty() {
                            found_slugs.push(slug);
                        }
                    }
                }
            }

            if read_executor && tables.contains("executor_metadata") {
                let mut e_stmt =
                    conn.prepare("SELECT data FROM executor_metadata ORDER BY rowid DESC LIMIT 1")?;
                let mut rows = e_stmt.query([])?;
                if let Some(row) = rows.next()?
                    && let Ok(bytes) = row.get::<_, Vec<u8>>(0)
                {
                    let root = decode_fields(&bytes);
                    let cc = proto_nested(&root, 10);
                    let cp = proto_nested(&cc, 2);
                    let lim = proto_num(&cp, 5);
                    if lim > 0 {
                        agy_max_token_limit = lim;
                    }
                }
            }

            Ok(())
        };

        let res = if retry_on_busy {
            with_readonly_retry(db_path, query_fn)
        } else {
            with_readonly_retry_schedule(db_path, &[], query_fn)
        };

        if let Err(err) = res
            && matches!(err, CoreError::DbBusy { .. })
        {
            db_busy = true;
        }
    }

    (found_slugs, agy_max_token_limit, db_busy)
}

fn pick_selected_slug(found_slugs: &[String]) -> String {
    found_slugs
        .iter()
        .find(|s| is_concrete_model_slug(s) && s.as_str() != "gemini-flash-worker-agy")
        .or_else(|| found_slugs.iter().find(|s| is_concrete_model_slug(s)))
        .or_else(|| found_slugs.first())
        .cloned()
        .unwrap_or_default()
}

/// Lightweight model label lookup for session lists.
/// Scans only recent `steps` and `gen_metadata` in `conversations/<conv_id>.db` without
/// falling back to `executor_metadata` or `antigravity_state.pbtxt`.
pub fn detect_conversation_model_label_from_db(
    app_data: &Path,
    conv_id: &str,
) -> (Option<String>, bool) {
    let db_path = app_data.join("conversations").join(format!("{conv_id}.db"));
    let (found_slugs, _, db_busy) = read_db_model_slugs(&db_path, 4, 3, false, false);
    let selected_slug = pick_selected_slug(&found_slugs);
    if selected_slug.is_empty() {
        (None, db_busy)
    } else {
        (Some(format_model_label(&selected_slug)), db_busy)
    }
}

pub fn detect_conversation_model(
    app_data: &Path,
    conv_id: &str,
    pbtxt_override: Option<&Path>,
) -> ModelDetectionResult {
    let db_path = app_data.join("conversations").join(format!("{conv_id}.db"));
    let pbtxt_path = pbtxt_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| app_data.join("antigravity_state.pbtxt"));

    let (found_slugs, agy_max_token_limit, db_busy) =
        read_db_model_slugs(&db_path, 8, 5, true, true);
    let mut selected_slug = pick_selected_slug(&found_slugs);
    let model_from_db = !selected_slug.is_empty();

    if selected_slug.is_empty()
        && let Ok(pbtxt) = std::fs::read_to_string(&pbtxt_path)
        && let Some(enum_name) = extract_pbtxt_last_selected_model(&pbtxt)
    {
        selected_slug = map_placeholder_enum_to_slug(&enum_name);
    }

    ModelDetectionResult {
        info: resolve_model_context_limit(&selected_slug, agy_max_token_limit),
        db_busy,
        model_from_db,
    }
}

pub fn extract_pbtxt_last_selected_model(pbtxt: &str) -> Option<String> {
    for line in pbtxt.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("last_selected_agent_model:") {
            let val = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}
