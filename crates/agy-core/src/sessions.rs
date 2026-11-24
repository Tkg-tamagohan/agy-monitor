use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::fingerprint::{DbFingerprint, TranscriptFingerprint};
use crate::paths::{
    AppDataRoot, GeminiPaths, Surface, normalize_file_uri, normalize_path_key, path_basename,
};
use crate::sqlite::with_readonly_retry;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub conversation_id: String,
    pub surface: Surface,
    pub app_data_dir: PathBuf,
    pub title: String,
    pub preview: String,
    pub display_title: String,
    pub agent_name: String,
    pub parent_conversation_id: String,
    pub root_session_id: String,
    pub root_title: String,
    pub is_subagent: bool,
    pub nesting_depth: u32,
    pub project_id: String,
    pub project_key: String,
    pub project_label: String,
    pub workspace_keys: Vec<String>,
    pub last_modified_ms: i64,
    pub effective_mtime_ms: i64,
    pub has_transcript: bool,
    pub has_db: bool,
    pub model_label: String,
}

#[derive(Debug, Clone)]
pub struct SessionTreeNode {
    pub summary: SessionSummary,
    pub children: Vec<SessionSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionIndex {
    pub projects: Vec<ProjectInfo>,
    pub roots: Vec<SessionTreeNode>,
    pub by_id: HashMap<String, SessionSummary>,
    pub db_busy: bool,
}

impl SessionIndex {
    pub fn latest_root_id(&self) -> Option<&str> {
        self.roots
            .iter()
            .find(|n| n.summary.has_transcript || n.summary.has_db)
            .or_else(|| self.roots.first())
            .map(|n| n.summary.conversation_id.as_str())
    }
}

#[derive(Debug, Clone)]
struct RawSummaryRow {
    conversation_id: String,
    surface: Surface,
    app_data_dir: PathBuf,
    title: String,
    preview: String,
    workspace_keys: Vec<String>,
    workspace_paths: Vec<PathBuf>,
    project_id: String,
    agent_name: String,
    parent_conversation_id: String,
    nesting_depth: u32,
    last_modified_ms: i64,
}

#[derive(Debug, Clone)]
struct ProjectEntry {
    id: String,
    name: String,
    folder_keys: Vec<String>,
}

pub fn is_valid_conv_id(id: &str) -> bool {
    id.len() >= 8 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// UTF-8 char-boundary safe truncation to `max_chars` characters.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = normalized.chars().count();
    if char_count <= max_chars {
        normalized
    } else {
        let mut out: String = normalized.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

pub fn parse_datetime_ms(raw: &str) -> i64 {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let normalized = if trimmed.len() >= 19 && trimmed.as_bytes().get(10) == Some(&b' ') {
        let mut s = trimmed.to_string();
        s.replace_range(10..11, "T");
        s
    } else {
        trimmed.to_string()
    };

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return dt.timestamp_millis();
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
        return naive.and_utc().timestamp_millis();
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return naive.and_utc().timestamp_millis();
    }
    0
}

pub fn parse_workspace_uris(raw_uris: &str) -> (Vec<String>, Vec<PathBuf>) {
    let trimmed = raw_uris.trim();
    if trimmed.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let Ok(uris) = serde_json::from_str::<Vec<String>>(trimmed) else {
        return (Vec::new(), Vec::new());
    };
    let mut keys = Vec::new();
    let mut paths = Vec::new();
    for uri in uris {
        let key = normalize_path_key(&uri);
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
            if let Some(pb) = normalize_file_uri(&uri) {
                paths.push(pb);
            }
        }
    }
    (keys, paths)
}

pub fn load_session_index(paths: &GeminiPaths) -> SessionIndex {
    let projects_json = paths.load_projects_json();
    let roots = paths.app_data_roots();

    let mut raw_map: HashMap<String, RawSummaryRow> = HashMap::new();
    let mut db_busy = false;

    for root in &roots {
        let (rows, busy) = read_summaries_for_root(root);
        if busy {
            db_busy = true;
        }
        for row in rows {
            let replace = match raw_map.get(&row.conversation_id) {
                None => true,
                Some(existing) => row.last_modified_ms >= existing.last_modified_ms,
            };
            if replace {
                raw_map.insert(row.conversation_id.clone(), row);
            }
        }
    }

    // Supplement unregistered conversations discovered under `<app_data>/brain/<id>`.
    for root in &roots {
        let brain_dir = root.dir.join("brain");
        let Ok(entries) = std::fs::read_dir(&brain_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !is_valid_conv_id(&id) {
                continue;
            }
            let logs_dir = entry.path().join(".system_generated").join("logs");
            let tfp = TranscriptFingerprint::from_logs_dir(&logs_dir);
            let conv_db = root.dir.join("conversations").join(format!("{id}.db"));
            let dfp = DbFingerprint::from_db_path(&conv_db);
            if !tfp.exists() && !dfp.exists() {
                continue;
            }
            if !raw_map.contains_key(&id) {
                let mtime_ms = tfp.latest_mtime_ms().max(dfp.latest_mtime_ms());
                raw_map.insert(
                    id.clone(),
                    RawSummaryRow {
                        conversation_id: id,
                        surface: root.surface,
                        app_data_dir: root.dir.clone(),
                        title: String::new(),
                        preview: String::new(),
                        workspace_keys: Vec::new(),
                        workspace_paths: Vec::new(),
                        project_id: String::new(),
                        agent_name: String::new(),
                        parent_conversation_id: String::new(),
                        nesting_depth: 0,
                        last_modified_ms: mtime_ms,
                    },
                );
            }
        }
    }

    // Build fallback project catalog from summaries + projects.json
    let mut project_by_id: HashMap<String, ProjectEntry> = HashMap::new();
    let mut project_by_path: HashMap<String, ProjectEntry> = HashMap::new();

    for row in raw_map.values() {
        let pid = row.project_id.trim();
        if !is_real_project_id(pid) {
            continue;
        }
        let entry = project_by_id
            .entry(pid.to_string())
            .or_insert_with(|| ProjectEntry {
                id: pid.to_string(),
                name: String::new(),
                folder_keys: Vec::new(),
            });
        for (idx, key) in row.workspace_keys.iter().enumerate() {
            if !entry.folder_keys.contains(key) {
                entry.folder_keys.push(key.clone());
            }
            if entry.name.is_empty()
                && let Some(name) = projects_json.get(key)
            {
                entry.name = name.clone();
            }
            if entry.name.is_empty()
                && let Some(pb) = row.workspace_paths.get(idx)
            {
                entry.name = path_basename(pb);
            }
        }
    }
    for entry in project_by_id.values() {
        if entry.name.is_empty() {
            continue;
        }
        for key in &entry.folder_keys {
            project_by_path
                .entry(key.clone())
                .or_insert_with(|| entry.clone());
        }
    }

    // Resolve parent chains (max 6 hops) and construct SessionSummary items.
    let ids: Vec<String> = raw_map.keys().cloned().collect();
    let mut summaries: HashMap<String, SessionSummary> = HashMap::with_capacity(ids.len());

    for id in &ids {
        let meta = &raw_map[id];
        let mut root_id = id.clone();
        let mut cur = meta;
        let mut inherited_project_id = meta.project_id.clone();
        let mut inherited_keys = meta.workspace_keys.clone();
        let mut inherited_paths = meta.workspace_paths.clone();
        let mut visited = HashSet::new();
        visited.insert(id.clone());

        let mut hops = 0;
        while !cur.parent_conversation_id.is_empty() && hops < 6 {
            let parent_id = cur.parent_conversation_id.clone();
            if !visited.insert(parent_id.clone()) {
                break;
            }
            root_id = parent_id.clone();
            if let Some(parent) = raw_map.get(&parent_id) {
                cur = parent;
                if is_real_project_id(&parent.project_id)
                    && !is_real_project_id(&inherited_project_id)
                {
                    inherited_project_id = parent.project_id.clone();
                }
                if inherited_keys.is_empty() && !parent.workspace_keys.is_empty() {
                    inherited_keys = parent.workspace_keys.clone();
                    inherited_paths = parent.workspace_paths.clone();
                }
            } else {
                break;
            }
            hops += 1;
        }

        let is_subagent = !meta.parent_conversation_id.is_empty();
        let display_title = resolve_display_title(meta, is_subagent);
        let root_title = if root_id == *id {
            display_title.clone()
        } else if let Some(root_raw) = raw_map.get(&root_id) {
            resolve_display_title(root_raw, false)
        } else {
            short_id(&root_id)
        };

        let (project_key, project_label) = resolve_session_project(
            &meta.project_id,
            &inherited_project_id,
            &inherited_keys,
            &inherited_paths,
            &projects_json,
            &project_by_id,
            &project_by_path,
        );

        let (effective_mtime_ms, has_transcript, has_db) = compute_effective_mtime_ms(
            &meta.app_data_dir,
            &meta.conversation_id,
            meta.last_modified_ms,
        );

        summaries.insert(
            id.clone(),
            SessionSummary {
                conversation_id: meta.conversation_id.clone(),
                surface: meta.surface,
                app_data_dir: meta.app_data_dir.clone(),
                title: meta.title.clone(),
                preview: meta.preview.clone(),
                display_title,
                agent_name: meta.agent_name.clone(),
                parent_conversation_id: meta.parent_conversation_id.clone(),
                root_session_id: root_id,
                root_title,
                is_subagent,
                nesting_depth: meta.nesting_depth,
                project_id: inherited_project_id,
                project_key,
                project_label,
                workspace_keys: inherited_keys,
                last_modified_ms: meta.last_modified_ms,
                effective_mtime_ms,
                has_transcript,
                has_db,
                model_label: String::new(),
            },
        );
    }

    // Group children under roots.
    let mut children_by_root: HashMap<String, Vec<SessionSummary>> = HashMap::new();
    let mut root_nodes: Vec<SessionTreeNode> = Vec::new();
    let mut seen_projects: HashMap<String, String> = HashMap::new();

    for summary in summaries.values() {
        if summary.project_key != "none" && !summary.project_label.is_empty() {
            seen_projects
                .entry(summary.project_key.clone())
                .or_insert_with(|| summary.project_label.clone());
        }
        if summary.is_subagent && summaries.contains_key(&summary.root_session_id) {
            children_by_root
                .entry(summary.root_session_id.clone())
                .or_default()
                .push(summary.clone());
        }
    }

    for summary in summaries.values() {
        if !summary.is_subagent || !summaries.contains_key(&summary.root_session_id) {
            let mut children = children_by_root
                .remove(&summary.conversation_id)
                .unwrap_or_default();
            children.sort_by(|a, b| {
                b.effective_mtime_ms
                    .cmp(&a.effective_mtime_ms)
                    .then_with(|| a.conversation_id.cmp(&b.conversation_id))
            });
            root_nodes.push(SessionTreeNode {
                summary: summary.clone(),
                children,
            });
        }
    }

    // Root effective mtime should also reflect newest child activity? Keep root's own effective_mtime_ms
    // so auto-follow follows root conversations by their own effective_mtime_ms (matching spec).
    root_nodes.sort_by(|a, b| {
        b.summary
            .effective_mtime_ms
            .cmp(&a.summary.effective_mtime_ms)
            .then_with(|| a.summary.conversation_id.cmp(&b.summary.conversation_id))
    });

    let mut projects: Vec<ProjectInfo> = seen_projects
        .into_iter()
        .map(|(key, label)| ProjectInfo { key, label })
        .collect();
    projects.sort_by_key(|a| a.label.to_lowercase());

    SessionIndex {
        projects,
        roots: root_nodes,
        by_id: summaries,
        db_busy,
    }
}

fn read_summaries_for_root(root: &AppDataRoot) -> (Vec<RawSummaryRow>, bool) {
    let db_path = root.dir.join("conversation_summaries.db");
    if !db_path.is_file() {
        return (Vec::new(), false);
    }

    let res = with_readonly_retry(&db_path, |conn| {
        let has_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='conversation_summaries')",
            [],
            |r| r.get(0),
        )?;
        if !has_table {
            return Ok(Vec::new());
        }

        let mut col_stmt = conn.prepare("PRAGMA table_info(conversation_summaries)")?;
        let cols: HashSet<String> = col_stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<HashSet<String>, rusqlite::Error>>(
        )?;

        let preview_expr = if cols.contains("preview") {
            "COALESCE(preview, '')"
        } else {
            "''"
        };
        let query = format!(
            "SELECT conversation_id,
                    COALESCE(title, ''),
                    {preview_expr},
                    COALESCE(workspace_uris, ''),
                    COALESCE(project_id, ''),
                    COALESCE(agent_name, ''),
                    COALESCE(parent_conversation_id, ''),
                    COALESCE(nesting_depth, 0),
                    COALESCE(last_modified_time, '')
             FROM conversation_summaries"
        );
        let mut stmt = conn.prepare(&query)?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let conv_id: String = row.get::<_, String>(0).unwrap_or_default();
            let conv_id = conv_id.trim().to_string();
            if conv_id.is_empty() {
                continue;
            }
            let title: String = row
                .get::<_, String>(1)
                .unwrap_or_default()
                .trim()
                .to_string();
            let preview: String = row
                .get::<_, String>(2)
                .unwrap_or_default()
                .trim()
                .to_string();
            let raw_uris: String = row.get::<_, String>(3).unwrap_or_default();
            let (workspace_keys, workspace_paths) = parse_workspace_uris(&raw_uris);
            let project_id: String = row
                .get::<_, String>(4)
                .unwrap_or_default()
                .trim()
                .to_string();
            let agent_name: String = row
                .get::<_, String>(5)
                .unwrap_or_default()
                .trim()
                .to_string();
            let parent_conversation_id: String = row
                .get::<_, String>(6)
                .unwrap_or_default()
                .trim()
                .to_string();
            let nesting_depth: u32 = row.get::<_, i64>(7).unwrap_or(0).max(0) as u32;
            let last_modified_ms = match row.get_ref(8) {
                Ok(rusqlite::types::ValueRef::Text(bytes)) => {
                    parse_datetime_ms(&String::from_utf8_lossy(bytes))
                }
                Ok(rusqlite::types::ValueRef::Integer(n)) => {
                    if n > 0 && n < 10_000_000_000 {
                        n * 1000
                    } else {
                        n.max(0)
                    }
                }
                _ => 0,
            };

            out.push(RawSummaryRow {
                conversation_id: conv_id,
                surface: root.surface,
                app_data_dir: root.dir.clone(),
                title,
                preview,
                workspace_keys,
                workspace_paths,
                project_id,
                agent_name,
                parent_conversation_id,
                nesting_depth,
                last_modified_ms,
            });
        }
        Ok(out)
    });

    match res {
        Ok(rows) => (rows, false),
        Err(CoreError::DbBusy { .. }) => (Vec::new(), true),
        Err(_) => (Vec::new(), false),
    }
}

fn resolve_display_title(row: &RawSummaryRow, is_subagent: bool) -> String {
    if is_subagent && !row.agent_name.trim().is_empty() {
        return row.agent_name.trim().to_string();
    }
    if !row.title.trim().is_empty() {
        return row.title.trim().to_string();
    }
    if !row.preview.trim().is_empty() {
        return truncate_chars(&row.preview, 60);
    }
    short_id(&row.conversation_id)
}

fn is_real_project_id(pid: &str) -> bool {
    let trimmed = pid.trim();
    !trimmed.is_empty() && trimmed != "outside-of-project" && trimmed != "default-cli-project"
}

fn resolve_session_project(
    own_project_id: &str,
    inherited_project_id: &str,
    workspace_keys: &[String],
    workspace_paths: &[PathBuf],
    projects_json: &HashMap<String, String>,
    project_by_id: &HashMap<String, ProjectEntry>,
    project_by_path: &HashMap<String, ProjectEntry>,
) -> (String, String) {
    // 1. Direct project_id lookup
    for pid in [own_project_id, inherited_project_id] {
        if is_real_project_id(pid)
            && let Some(entry) = project_by_id.get(pid)
            && !entry.name.is_empty()
        {
            return (entry.id.clone(), entry.name.clone());
        }
    }

    // 2. Workspace URI match against projects.json or project_by_path
    for (idx, ws_key) in workspace_keys.iter().enumerate() {
        if let Some(name) = projects_json.get(ws_key) {
            return (format!("path:{ws_key}"), name.clone());
        }
        if let Some(entry) = project_by_path.get(ws_key) {
            return (entry.id.clone(), entry.name.clone());
        }
        for (proj_key, name) in projects_json {
            if ws_key.starts_with(&format!("{proj_key}/")) {
                return (format!("path:{proj_key}"), name.clone());
            }
        }
        for (proj_key, entry) in project_by_path {
            if ws_key.starts_with(&format!("{proj_key}/")) {
                return (entry.id.clone(), entry.name.clone());
            }
        }
        if let Some(pb) = workspace_paths.get(idx) {
            let base = path_basename(pb);
            if !base.is_empty() {
                return (format!("path:{ws_key}"), base);
            }
        }
    }

    ("none".to_string(), String::new())
}

pub fn compute_effective_mtime_ms(
    app_data_dir: &Path,
    conv_id: &str,
    db_modified_ms: i64,
) -> (i64, bool, bool) {
    let logs_dir = app_data_dir
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs");
    let tfp = TranscriptFingerprint::from_logs_dir(&logs_dir);
    let conv_db = app_data_dir
        .join("conversations")
        .join(format!("{conv_id}.db"));
    let dfp = DbFingerprint::from_db_path(&conv_db);
    let effective = db_modified_ms
        .max(tfp.latest_mtime_ms())
        .max(dfp.latest_mtime_ms());
    (effective, tfp.exists(), dfp.exists())
}
