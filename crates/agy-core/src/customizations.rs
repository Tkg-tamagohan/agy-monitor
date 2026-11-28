use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use std::fmt;

use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::analyzer::{CustomizationBreakdown, est_text_tokens};
use crate::paths::GeminiPaths;

pub const MAX_SCAN_FILES: usize = 2000;
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_SCAN_DEPTH: usize = 8;
pub const MAX_WARNINGS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CustomizationKind {
    McpTool,
    McpInstruction,
    RuleFile,
    SkillIndex,
    CustomAgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoadTiming {
    Always,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomizationItem {
    pub kind: CustomizationKind,
    pub timing: LoadTiming,
    pub name: String,
    pub source: String,
    pub path: PathBuf,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomizationInventory {
    pub items: Vec<CustomizationItem>,
    pub breakdown: CustomizationBreakdown,
    pub mcp_server_count: usize,
    pub plugin_count: usize,
    pub scan_warnings: Vec<String>,
}

pub fn scan_customizations(paths: &GeminiPaths) -> CustomizationInventory {
    let cancel = AtomicBool::new(false);
    scan_customizations_cancellable(paths, &cancel).unwrap_or_default()
}

pub fn scan_customizations_cancellable(
    paths: &GeminiPaths,
    cancel: &AtomicBool,
) -> Option<CustomizationInventory> {
    let mut ctx = ScanContext::new(&paths.gemini_dir, cancel);
    if ctx.is_cancelled() {
        return None;
    }

    // 1. antigravity/mcp/<server>/ and antigravity/builtin/skills/*/SKILL.md
    let antigravity_dir = paths.gemini_dir.join("antigravity");
    if ctx.enter_dir(&antigravity_dir) {
        let mcp_dir = antigravity_dir.join("mcp");
        if ctx.enter_dir(&mcp_dir) && !scan_antigravity_mcp(&mut ctx, &mcp_dir, 2) {
            return None;
        }

        let builtin_skills_dir = antigravity_dir.join("builtin").join("skills");
        if ctx.enter_dir(&antigravity_dir.join("builtin"))
            && ctx.enter_dir(&builtin_skills_dir)
            && !scan_skills_dir(&mut ctx, &builtin_skills_dir, "builtin", 3)
        {
            return None;
        }

        let antigravity_mcp_cfg = antigravity_dir.join("mcp_config.json");
        if !scan_mcp_config_file(
            &mut ctx,
            &antigravity_mcp_cfg,
            false,
            None,
            PRECEDENCE_ANTIGRAVITY_ROOT,
        ) {
            return None;
        }
    }

    if ctx.is_cancelled() {
        return None;
    }

    // 2. config/plugins/<plugin>/ and config/mcp_config.json
    let config_dir = paths.gemini_dir.join("config");
    if ctx.enter_dir(&config_dir) {
        let plugins_dir = config_dir.join("plugins");
        if ctx.enter_dir(&plugins_dir) && !scan_plugins_dir(&mut ctx, &plugins_dir, 2) {
            return None;
        }

        let config_mcp_cfg = config_dir.join("mcp_config.json");
        if !scan_mcp_config_file(
            &mut ctx,
            &config_mcp_cfg,
            false,
            None,
            PRECEDENCE_CONFIG_ROOT,
        ) {
            return None;
        }
    }

    if ctx.is_cancelled() {
        return None;
    }

    Some(ctx.finish())
}

const PRECEDENCE_PLUGIN: u8 = 0;
const PRECEDENCE_CONFIG_ROOT: u8 = 1;
const PRECEDENCE_ANTIGRAVITY_ROOT: u8 = 2;

struct PendingMcpTool {
    server_name: String,
    name: String,
    file_stem: String,
    source: String,
    path: PathBuf,
    estimated_tokens: u64,
    explicit_deferred: bool,
}

struct ScanContext<'a> {
    gemini_dir: &'a Path,
    cancel: &'a AtomicBool,
    items: Vec<CustomizationItem>,
    pending_mcp_tools: Vec<PendingMcpTool>,
    mcp_tool_eager: HashMap<(String, String), (u8, bool)>,
    breakdown: CustomizationBreakdown,
    mcp_servers: HashSet<String>,
    plugins: HashSet<String>,
    warnings: Vec<String>,
    files_scanned: usize,
    file_limit_warned: bool,
}

impl<'a> ScanContext<'a> {
    fn new(gemini_dir: &'a Path, cancel: &'a AtomicBool) -> Self {
        Self {
            gemini_dir,
            cancel,
            items: Vec::new(),
            pending_mcp_tools: Vec::new(),
            mcp_tool_eager: HashMap::new(),
            breakdown: CustomizationBreakdown::default(),
            mcp_servers: HashSet::new(),
            plugins: HashSet::new(),
            warnings: Vec::new(),
            files_scanned: 0,
            file_limit_warned: false,
        }
    }

    fn record_mcp_tool_eager(
        &mut self,
        server_key: String,
        tool_name: String,
        eager: bool,
        precedence: u8,
    ) {
        let key = (server_key, tool_name);
        match self.mcp_tool_eager.get_mut(&key) {
            Some((existing_prec, existing_eager)) => {
                if precedence >= *existing_prec {
                    *existing_prec = precedence;
                    *existing_eager = eager;
                }
            }
            None => {
                self.mcp_tool_eager.insert(key, (precedence, eager));
            }
        }
    }

    fn lookup_mcp_tool_eager(
        &self,
        server_name: &str,
        tool_name: &str,
        file_stem: &str,
    ) -> Option<bool> {
        if let Some((_, eager)) = self
            .mcp_tool_eager
            .get(&(server_name.to_string(), tool_name.to_string()))
        {
            return Some(*eager);
        }
        if file_stem != tool_name
            && let Some((_, eager)) = self
                .mcp_tool_eager
                .get(&(server_name.to_string(), file_stem.to_string()))
        {
            return Some(*eager);
        }
        None
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn limit_reached(&self) -> bool {
        self.files_scanned >= MAX_SCAN_FILES
    }

    fn rel_display(&self, path: &Path) -> String {
        path.strip_prefix(self.gemini_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn push_warning(&mut self, msg: String) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(msg);
        }
    }

    fn push_item(&mut self, item: CustomizationItem) {
        match (item.kind, item.timing) {
            (CustomizationKind::McpTool, LoadTiming::Always) => {
                self.breakdown.mcp_tools += item.estimated_tokens;
            }
            (CustomizationKind::McpTool, LoadTiming::Deferred) => {
                self.breakdown.mcp_deferred += item.estimated_tokens;
            }
            (CustomizationKind::McpInstruction, LoadTiming::Always) => {
                self.breakdown.mcp_instructions += item.estimated_tokens;
            }
            (CustomizationKind::McpInstruction, LoadTiming::Deferred) => {
                self.breakdown.mcp_deferred += item.estimated_tokens;
            }
            (CustomizationKind::RuleFile, LoadTiming::Always) => {
                self.breakdown.rules_files += item.estimated_tokens;
            }
            (CustomizationKind::RuleFile, LoadTiming::Deferred) => {
                self.breakdown.rules_deferred += item.estimated_tokens;
            }
            (CustomizationKind::SkillIndex, LoadTiming::Always) => {
                self.breakdown.skills_index += item.estimated_tokens;
            }
            (CustomizationKind::SkillIndex, LoadTiming::Deferred) => {
                self.breakdown.skills_deferred += item.estimated_tokens;
            }
            (CustomizationKind::CustomAgent, _) => {
                self.breakdown.custom_agents += item.estimated_tokens;
            }
        }
        self.items.push(item);
    }

    fn enter_dir(&mut self, path: &Path) -> bool {
        if self.is_cancelled() || self.limit_reached() {
            return false;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    let rel = self.rel_display(path);
                    self.push_warning(format!("{rel}: skipped symlink directory"));
                    return false;
                }
                meta.file_type().is_dir()
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => {
                let rel = self.rel_display(path);
                self.push_warning(format!(
                    "{rel}: failed to stat directory ({:?})",
                    err.kind()
                ));
                false
            }
        }
    }

    fn read_dir_sorted(&mut self, dir: &Path) -> Option<Vec<(String, PathBuf, std::fs::FileType)>> {
        if self.is_cancelled() {
            return None;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(err) => {
                let rel = self.rel_display(dir);
                self.push_warning(format!(
                    "{rel}: failed to read directory ({:?})",
                    err.kind()
                ));
                return Some(Vec::new());
            }
        };

        let mut list = Vec::new();
        for entry_res in entries {
            if self.is_cancelled() {
                return None;
            }
            let Ok(entry) = entry_res else {
                let rel = self.rel_display(dir);
                self.push_warning(format!("{rel}: failed to read directory entry"));
                continue;
            };
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(err) => {
                    let rel = self.rel_display(&path);
                    self.push_warning(format!(
                        "{rel}: failed to read file type ({:?})",
                        err.kind()
                    ));
                    continue;
                }
            };
            if ft.is_symlink() {
                let rel = self.rel_display(&path);
                self.push_warning(format!("{rel}: skipped symlink"));
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            list.push((name, path, ft));
        }
        list.sort_by(|a, b| a.0.cmp(&b.0));
        Some(list)
    }

    fn read_safe_utf8_file(&mut self, path: &Path) -> ReadOutcome {
        if self.is_cancelled() {
            return ReadOutcome::Cancelled;
        }
        if self.files_scanned >= MAX_SCAN_FILES {
            if !self.file_limit_warned {
                self.file_limit_warned = true;
                self.push_warning(format!(
                    "scan file limit reached ({MAX_SCAN_FILES}); remaining files skipped"
                ));
            }
            return ReadOutcome::LimitReached;
        }
        self.files_scanned += 1;

        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return ReadOutcome::Missing,
            Err(err) => {
                let rel = self.rel_display(path);
                self.push_warning(format!("{rel}: failed to stat file ({:?})", err.kind()));
                return ReadOutcome::Skipped;
            }
        };

        if meta.file_type().is_symlink() {
            let rel = self.rel_display(path);
            self.push_warning(format!("{rel}: skipped symlink"));
            return ReadOutcome::Skipped;
        }
        if !meta.file_type().is_file() {
            return ReadOutcome::Skipped;
        }
        if meta.len() > MAX_FILE_BYTES {
            let rel = self.rel_display(path);
            self.push_warning(format!(
                "{rel}: skipped oversized file ({} bytes > {MAX_FILE_BYTES} limit)",
                meta.len()
            ));
            return ReadOutcome::Skipped;
        }

        let file = match File::open(path) {
            Ok(f) => f,
            Err(err) => {
                let rel = self.rel_display(path);
                self.push_warning(format!("{rel}: failed to open file ({:?})", err.kind()));
                return ReadOutcome::Skipped;
            }
        };

        let mut buf = Vec::with_capacity(meta.len().min(MAX_FILE_BYTES) as usize);
        if let Err(err) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut buf) {
            let rel = self.rel_display(path);
            self.push_warning(format!("{rel}: failed to read file ({:?})", err.kind()));
            return ReadOutcome::Skipped;
        }
        if buf.len() as u64 > MAX_FILE_BYTES {
            let rel = self.rel_display(path);
            self.push_warning(format!(
                "{rel}: skipped oversized file (> {MAX_FILE_BYTES} limit)"
            ));
            return ReadOutcome::Skipped;
        }

        match String::from_utf8(buf) {
            Ok(text) => ReadOutcome::Ok(text),
            Err(_) => {
                let rel = self.rel_display(path);
                self.push_warning(format!("{rel}: skipped non-UTF-8 file"));
                ReadOutcome::Skipped
            }
        }
    }

    fn finish(mut self) -> CustomizationInventory {
        let pending_tools = std::mem::take(&mut self.pending_mcp_tools);
        for tool in pending_tools {
            let timing = if tool.explicit_deferred {
                LoadTiming::Deferred
            } else if self.lookup_mcp_tool_eager(&tool.server_name, &tool.name, &tool.file_stem)
                == Some(true)
            {
                LoadTiming::Always
            } else {
                LoadTiming::Deferred
            };
            self.push_item(CustomizationItem {
                kind: CustomizationKind::McpTool,
                timing,
                name: tool.name,
                source: tool.source,
                path: tool.path,
                estimated_tokens: tool.estimated_tokens,
            });
        }

        self.items.sort_by(|a, b| {
            b.estimated_tokens
                .cmp(&a.estimated_tokens)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.path.cmp(&b.path))
        });

        CustomizationInventory {
            items: self.items,
            breakdown: self.breakdown,
            mcp_server_count: self.mcp_servers.len(),
            plugin_count: self.plugins.len(),
            scan_warnings: self.warnings,
        }
    }
}

enum ReadOutcome {
    Ok(String),
    Missing,
    Skipped,
    LimitReached,
    Cancelled,
}

#[derive(Deserialize)]
struct McpToolMetadata {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    lazy: Option<bool>,
    #[serde(default)]
    defer: Option<bool>,
}

#[derive(Deserialize)]
struct McpConfigKeysOnly {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Option<HashMap<String, McpServerConfigSafe>>,
    #[serde(flatten)]
    top_level: HashMap<String, McpServerConfigSafe>,
}

fn drain_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

#[derive(Default)]
struct McpServerConfigSafe {
    tool_eager: HashMap<String, bool>,
}

impl<'de> Deserialize<'de> for McpServerConfigSafe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ServerVisitor;
        impl<'de> Visitor<'de> for ServerVisitor {
            type Value = McpServerConfigSafe;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("MCP server configuration object")
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                drain_seq(seq)?;
                Ok(Self::Value::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut tool_eager = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "tools" {
                        tool_eager = map.next_value::<McpToolsMapSafe>()?.0;
                    } else {
                        let _ = map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(McpServerConfigSafe { tool_eager })
            }
        }
        deserializer.deserialize_any(ServerVisitor)
    }
}

#[derive(Default)]
struct McpToolsMapSafe(HashMap<String, bool>);

impl<'de> Deserialize<'de> for McpToolsMapSafe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ToolsMapVisitor;
        impl<'de> Visitor<'de> for ToolsMapVisitor {
            type Value = McpToolsMapSafe;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("MCP tools map")
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                drain_seq(seq)?;
                Ok(Self::Value::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = HashMap::new();
                while let Some(raw_name) = map.next_key::<String>()? {
                    let entry = map.next_value::<McpToolEntrySafe>()?;
                    if let Some(eager) = entry.eager {
                        let trimmed = raw_name.trim();
                        if !trimmed.is_empty() {
                            out.insert(trimmed.to_string(), eager);
                        }
                    }
                }
                Ok(McpToolsMapSafe(out))
            }
        }
        deserializer.deserialize_any(ToolsMapVisitor)
    }
}

#[derive(Default)]
struct McpToolEntrySafe {
    eager: Option<bool>,
}

impl<'de> Deserialize<'de> for McpToolEntrySafe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryVisitor;
        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = McpToolEntrySafe;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("MCP tool entry object")
            }

            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Self::Value::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                drain_seq(seq)?;
                Ok(Self::Value::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut eager = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "eager" {
                        eager = map.next_value::<OptionalBoolSafe>()?.0;
                    } else {
                        let _ = map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(McpToolEntrySafe { eager })
            }
        }
        deserializer.deserialize_any(EntryVisitor)
    }
}

#[derive(Default)]
struct OptionalBoolSafe(Option<bool>);

impl<'de> Deserialize<'de> for OptionalBoolSafe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoolVisitor;
        impl<'de> Visitor<'de> for BoolVisitor {
            type Value = OptionalBoolSafe;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("optional boolean")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(Some(v)))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(None))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OptionalBoolSafe(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                drain_seq(seq)?;
                Ok(OptionalBoolSafe(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                drain_map(map)?;
                Ok(OptionalBoolSafe(None))
            }
        }
        deserializer.deserialize_any(BoolVisitor)
    }
}

fn classify_json_error(err: &serde_json::Error) -> &'static str {
    match err.classify() {
        serde_json::error::Category::Io => "io error",
        serde_json::error::Category::Syntax => "syntax error",
        serde_json::error::Category::Data => "invalid schema",
        serde_json::error::Category::Eof => "unexpected EOF",
    }
}

fn scan_antigravity_mcp(ctx: &mut ScanContext<'_>, mcp_dir: &Path, depth: usize) -> bool {
    let Some(entries) = ctx.read_dir_sorted(mcp_dir) else {
        return false;
    };
    for (server_name, server_path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if !ft.is_dir() {
            continue;
        }
        ctx.mcp_servers.insert(server_name.clone());
        if !scan_mcp_server_dir(ctx, &server_name, &server_path, depth + 1) {
            return false;
        }
    }
    true
}

fn scan_mcp_server_dir(
    ctx: &mut ScanContext<'_>,
    server_name: &str,
    dir: &Path,
    depth: usize,
) -> bool {
    if depth > MAX_SCAN_DEPTH {
        let rel = ctx.rel_display(dir);
        ctx.push_warning(format!(
            "{rel}: skipped directory exceeding max depth {MAX_SCAN_DEPTH}"
        ));
        return true;
    }
    let Some(entries) = ctx.read_dir_sorted(dir) else {
        return false;
    };
    let source = format!("mcp:{server_name}");
    for (file_name, path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if ft.is_dir() {
            if !scan_mcp_server_dir(ctx, server_name, &path, depth + 1) {
                return false;
            }
            continue;
        }
        if !ft.is_file() {
            continue;
        }

        if file_name.eq_ignore_ascii_case("instructions.md") {
            let text = match ctx.read_safe_utf8_file(&path) {
                ReadOutcome::Ok(t) => t,
                ReadOutcome::Cancelled => return false,
                ReadOutcome::LimitReached => break,
                ReadOutcome::Missing | ReadOutcome::Skipped => continue,
            };
            let estimated_tokens = est_text_tokens(&text);
            ctx.push_item(CustomizationItem {
                kind: CustomizationKind::McpInstruction,
                timing: LoadTiming::Always,
                name: "instructions.md".to_string(),
                source: source.clone(),
                path,
                estimated_tokens,
            });
        } else if has_ascii_extension(&path, "json") {
            let text = match ctx.read_safe_utf8_file(&path) {
                ReadOutcome::Ok(t) => t,
                ReadOutcome::Cancelled => return false,
                ReadOutcome::LimitReached => break,
                ReadOutcome::Missing | ReadOutcome::Skipped => continue,
            };
            let meta = match serde_json::from_str::<McpToolMetadata>(&text) {
                Ok(m) => m,
                Err(err) => {
                    let rel = ctx.rel_display(&path);
                    ctx.push_warning(format!(
                        "{rel}: invalid JSON ({})",
                        classify_json_error(&err)
                    ));
                    continue;
                }
            };
            let explicit_deferred = meta.lazy == Some(true) || meta.defer == Some(true);
            let file_stem = file_stem_or_name(&path);
            let name = meta
                .name
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| file_stem.clone());
            let estimated_tokens = est_text_tokens(&text);
            ctx.pending_mcp_tools.push(PendingMcpTool {
                server_name: server_name.to_string(),
                name,
                file_stem,
                source: source.clone(),
                path,
                estimated_tokens,
                explicit_deferred,
            });
        }
    }
    true
}

fn scan_skills_dir(
    ctx: &mut ScanContext<'_>,
    skills_dir: &Path,
    source: &str,
    depth: usize,
) -> bool {
    if depth >= MAX_SCAN_DEPTH {
        let rel = ctx.rel_display(skills_dir);
        ctx.push_warning(format!(
            "{rel}: skipped directory exceeding max depth {MAX_SCAN_DEPTH}"
        ));
        return true;
    }
    let Some(entries) = ctx.read_dir_sorted(skills_dir) else {
        return false;
    };
    for (skill_dir_name, skill_dir_path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if !ft.is_dir() {
            continue;
        }
        let skill_md = skill_dir_path.join("SKILL.md");
        let text = match ctx.read_safe_utf8_file(&skill_md) {
            ReadOutcome::Ok(t) => t,
            ReadOutcome::Cancelled => return false,
            ReadOutcome::LimitReached => break,
            ReadOutcome::Missing | ReadOutcome::Skipped => continue,
        };

        let (fm_opt, body) = split_frontmatter(&text);
        let (fm_name, fm_desc, fm_raw) = match fm_opt {
            Some(ref fm) => (
                fm.name.as_deref().map(str::trim).filter(|s| !s.is_empty()),
                fm.description
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                fm.raw.trim(),
            ),
            None => (None, None, ""),
        };

        let skill_name = fm_name.unwrap_or(&skill_dir_name).to_string();
        let index_text = match (fm_name, fm_desc) {
            (Some(n), Some(d)) => format!("name: {n}\ndescription: {d}"),
            (Some(n), None) => format!("name: {n}"),
            (None, Some(d)) => format!("description: {d}"),
            (None, None) => fm_raw.to_string(),
        };

        let index_tokens = if index_text.trim().is_empty() {
            0
        } else {
            est_text_tokens(&index_text)
        };
        let body_tokens = if body.trim().is_empty() {
            0
        } else {
            est_text_tokens(body.trim())
        };

        if index_tokens > 0 {
            ctx.push_item(CustomizationItem {
                kind: CustomizationKind::SkillIndex,
                timing: LoadTiming::Always,
                name: skill_name.clone(),
                source: source.to_string(),
                path: skill_md.clone(),
                estimated_tokens: index_tokens,
            });
        }
        if body_tokens > 0 {
            ctx.push_item(CustomizationItem {
                kind: CustomizationKind::SkillIndex,
                timing: LoadTiming::Deferred,
                name: format!("{skill_name} (body)"),
                source: source.to_string(),
                path: skill_md,
                estimated_tokens: body_tokens,
            });
        }
    }
    true
}

fn scan_plugins_dir(ctx: &mut ScanContext<'_>, plugins_dir: &Path, depth: usize) -> bool {
    let Some(entries) = ctx.read_dir_sorted(plugins_dir) else {
        return false;
    };
    for (plugin_name, plugin_path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if !ft.is_dir() {
            continue;
        }
        ctx.plugins.insert(plugin_name.clone());
        let source = format!("plugin:{plugin_name}");
        let plugin_depth = depth + 1;

        let rules_dir = plugin_path.join("rules");
        if ctx.enter_dir(&rules_dir)
            && !scan_plugin_rules_dir(ctx, &rules_dir, &source, plugin_depth + 1)
        {
            return false;
        }

        let skills_dir = plugin_path.join("skills");
        if ctx.enter_dir(&skills_dir)
            && !scan_skills_dir(ctx, &skills_dir, &source, plugin_depth + 1)
        {
            return false;
        }

        for agents_sub in ["agents", "subagents"] {
            let agents_dir = plugin_path.join(agents_sub);
            if ctx.enter_dir(&agents_dir)
                && !scan_plugin_agents_dir(ctx, &agents_dir, &source, plugin_depth + 1)
            {
                return false;
            }
        }

        for mcp_file in [".mcp.json", "mcp_config.json", "mcp.json"] {
            let path = plugin_path.join(mcp_file);
            let allow_top_level = mcp_file == ".mcp.json";
            if !scan_mcp_config_file(
                ctx,
                &path,
                allow_top_level,
                Some(&plugin_name),
                PRECEDENCE_PLUGIN,
            ) {
                return false;
            }
        }

        let plugin_mcp_dir = plugin_path.join("mcp");
        if ctx.enter_dir(&plugin_mcp_dir)
            && !scan_plugin_mcp_dir(ctx, &plugin_name, &plugin_mcp_dir, plugin_depth + 1)
        {
            return false;
        }
    }
    true
}

fn scan_plugin_rules_dir(
    ctx: &mut ScanContext<'_>,
    dir: &Path,
    source: &str,
    depth: usize,
) -> bool {
    if depth > MAX_SCAN_DEPTH {
        let rel = ctx.rel_display(dir);
        ctx.push_warning(format!(
            "{rel}: skipped directory exceeding max depth {MAX_SCAN_DEPTH}"
        ));
        return true;
    }
    let Some(entries) = ctx.read_dir_sorted(dir) else {
        return false;
    };
    for (_file_name, path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if ft.is_dir() {
            if !scan_plugin_rules_dir(ctx, &path, source, depth + 1) {
                return false;
            }
            continue;
        }
        if !ft.is_file() || !has_ascii_extension(&path, "md") {
            continue;
        }

        let text = match ctx.read_safe_utf8_file(&path) {
            ReadOutcome::Ok(t) => t,
            ReadOutcome::Cancelled => return false,
            ReadOutcome::LimitReached => break,
            ReadOutcome::Missing | ReadOutcome::Skipped => continue,
        };

        let (fm_opt, _) = split_frontmatter(&text);
        let timing = classify_rule_timing(fm_opt.as_ref());
        let estimated_tokens = est_text_tokens(&text);
        let name = file_stem_or_name(&path);

        ctx.push_item(CustomizationItem {
            kind: CustomizationKind::RuleFile,
            timing,
            name,
            source: source.to_string(),
            path,
            estimated_tokens,
        });
    }
    true
}

fn scan_plugin_agents_dir(
    ctx: &mut ScanContext<'_>,
    dir: &Path,
    source: &str,
    depth: usize,
) -> bool {
    if depth > MAX_SCAN_DEPTH {
        let rel = ctx.rel_display(dir);
        ctx.push_warning(format!(
            "{rel}: skipped directory exceeding max depth {MAX_SCAN_DEPTH}"
        ));
        return true;
    }
    let Some(entries) = ctx.read_dir_sorted(dir) else {
        return false;
    };
    for (_file_name, path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if ft.is_dir() {
            if !scan_plugin_agents_dir(ctx, &path, source, depth + 1) {
                return false;
            }
            continue;
        }
        if !ft.is_file() || !has_ascii_extension(&path, "md") {
            continue;
        }

        let text = match ctx.read_safe_utf8_file(&path) {
            ReadOutcome::Ok(t) => t,
            ReadOutcome::Cancelled => return false,
            ReadOutcome::LimitReached => break,
            ReadOutcome::Missing | ReadOutcome::Skipped => continue,
        };

        let (fm_opt, _) = split_frontmatter(&text);
        let name = fm_opt
            .and_then(|fm| fm.name)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| file_stem_or_name(&path));
        let estimated_tokens = est_text_tokens(&text);

        ctx.push_item(CustomizationItem {
            kind: CustomizationKind::CustomAgent,
            timing: LoadTiming::Always,
            name,
            source: source.to_string(),
            path,
            estimated_tokens,
        });
    }
    true
}

fn scan_plugin_mcp_dir(
    ctx: &mut ScanContext<'_>,
    plugin_name: &str,
    mcp_dir: &Path,
    depth: usize,
) -> bool {
    if depth > MAX_SCAN_DEPTH {
        let rel = ctx.rel_display(mcp_dir);
        ctx.push_warning(format!(
            "{rel}: skipped directory exceeding max depth {MAX_SCAN_DEPTH}"
        ));
        return true;
    }
    let Some(entries) = ctx.read_dir_sorted(mcp_dir) else {
        return false;
    };
    let is_node_bundle = entries
        .iter()
        .any(|(name, _, ft)| ft.is_file() && name == "package.json");

    for (name, path, ft) in entries {
        if ctx.is_cancelled() {
            return false;
        }
        if ctx.limit_reached() {
            break;
        }
        if ft.is_file() {
            if name == ".mcp.json" || name == "mcp_config.json" || name == "mcp.json" {
                let allow_top_level = name == ".mcp.json";
                if !scan_mcp_config_file(
                    ctx,
                    &path,
                    allow_top_level,
                    Some(plugin_name),
                    PRECEDENCE_PLUGIN,
                ) {
                    return false;
                }
            } else if !is_node_bundle
                && has_ascii_extension(&path, "json")
                && !scan_mcp_config_file(ctx, &path, false, Some(plugin_name), PRECEDENCE_PLUGIN)
            {
                return false;
            }
        } else if ft.is_dir() && !is_node_bundle && !is_ignored_code_subdir(&name) {
            ctx.mcp_servers.insert(name);
        }
    }
    true
}

fn is_ignored_code_subdir(name: &str) -> bool {
    matches!(
        name,
        "bin" | "dist" | "node_modules" | "src" | "build" | "lib"
    )
}

fn scan_mcp_config_file(
    ctx: &mut ScanContext<'_>,
    path: &Path,
    allow_top_level: bool,
    plugin_prefix: Option<&str>,
    precedence: u8,
) -> bool {
    let text = match ctx.read_safe_utf8_file(path) {
        ReadOutcome::Ok(t) => t,
        ReadOutcome::Cancelled => return false,
        ReadOutcome::Missing | ReadOutcome::Skipped | ReadOutcome::LimitReached => return true,
    };

    let parsed = match serde_json::from_str::<McpConfigKeysOnly>(&text) {
        Ok(p) => p,
        Err(err) => {
            let rel = ctx.rel_display(path);
            ctx.push_warning(format!(
                "{rel}: invalid JSON ({})",
                classify_json_error(&err)
            ));
            return true;
        }
    };

    let mut register_servers = |servers: HashMap<String, McpServerConfigSafe>,
                                skip_dollar: bool| {
        for (key, server_cfg) in servers {
            let trimmed = key.trim();
            if trimmed.is_empty() || (skip_dollar && trimmed.starts_with('$')) {
                continue;
            }
            ctx.mcp_servers.insert(trimmed.to_string());
            if !server_cfg.tool_eager.is_empty() {
                let server_lookup_key = match plugin_prefix {
                    Some(prefix) => format!("{prefix}_{trimmed}"),
                    None => trimmed.to_string(),
                };
                for (tool_name, eager) in server_cfg.tool_eager {
                    ctx.record_mcp_tool_eager(
                        server_lookup_key.clone(),
                        tool_name,
                        eager,
                        precedence,
                    );
                }
            }
        }
    };

    if let Some(servers) = parsed.mcp_servers {
        register_servers(servers, false);
    } else if allow_top_level {
        register_servers(parsed.top_level, true);
    }

    true
}

struct Frontmatter<'a> {
    raw: &'a str,
    name: Option<String>,
    description: Option<String>,
    always_apply: Option<bool>,
    has_globs: bool,
    has_paths: bool,
    has_description: bool,
    trigger: Option<String>,
}

fn split_frontmatter(raw: &str) -> (Option<Frontmatter<'_>>, &str) {
    let content = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    let mut lines = content.split('\n');
    let Some(first_line) = lines.next() else {
        return (None, content);
    };
    if first_line.trim_end_matches('\r').trim() != "---" {
        return (None, content);
    }

    let fm_start = first_line.len() + 1;
    let mut offset = fm_start;
    for line in lines {
        let line_len = line.len();
        if line.trim_end_matches('\r').trim() == "---" {
            let fm_raw = &content[fm_start..offset];
            let body_start = offset + line_len;
            let body = if body_start < content.len() {
                &content[body_start + 1..]
            } else {
                ""
            };
            return (Some(parse_frontmatter(fm_raw)), body);
        }
        offset += line_len + 1;
    }

    (None, content)
}

fn parse_frontmatter(fm_raw: &str) -> Frontmatter<'_> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut always_apply: Option<bool> = None;
    let mut has_globs = false;
    let mut has_paths = false;
    let mut has_description = false;
    let mut trigger: Option<String> = None;

    let lines: Vec<&str> = fm_raw
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    let mut idx = 0usize;
    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if is_indented {
            idx += 1;
            continue;
        }

        let Some((raw_key, raw_rest)) = line.split_once(':') else {
            idx += 1;
            continue;
        };
        let key = raw_key.trim();
        let val_inline = strip_yaml_inline_comment(raw_rest.trim());

        let (val_combined, next_idx) = collect_yaml_value(&lines, idx + 1, val_inline);
        idx = next_idx;

        match key {
            "name" => {
                let unquoted = unquote_yaml_scalar(&val_combined);
                if !unquoted.is_empty() {
                    name = Some(unquoted);
                }
            }
            "description" => {
                let unquoted = unquote_yaml_scalar(&val_combined);
                if !unquoted.is_empty() {
                    has_description = true;
                    description = Some(unquoted);
                }
            }
            "alwaysApply" | "always_apply" => {
                let lower = unquote_yaml_scalar(&val_combined).to_ascii_lowercase();
                if lower == "true" {
                    always_apply = Some(true);
                } else if lower == "false" {
                    always_apply = Some(false);
                }
            }
            "globs" => {
                if !is_empty_yaml_value(&val_combined) {
                    has_globs = true;
                }
            }
            "paths" => {
                if !is_empty_yaml_value(&val_combined) {
                    has_paths = true;
                }
            }
            "trigger" => {
                let unquoted = unquote_yaml_scalar(&val_combined);
                if !unquoted.is_empty() {
                    trigger = Some(unquoted);
                }
            }
            _ => {}
        }
    }

    Frontmatter {
        raw: fm_raw,
        name,
        description,
        always_apply,
        has_globs,
        has_paths,
        has_description,
        trigger,
    }
}

fn collect_yaml_value(lines: &[&str], mut idx: usize, inline: &str) -> (String, usize) {
    let is_block_scalar = matches!(inline, "|" | ">" | "|-" | ">-" | "|+" | ">+");
    let mut parts: Vec<String> = Vec::new();
    let mut quote_state = (false, false, false);
    if !is_block_scalar && !inline.is_empty() {
        let (_, next_state) = scan_yaml_quotes(inline, quote_state, false);
        quote_state = next_state;
        parts.push(inline.to_string());
    }

    while idx < lines.len() {
        let next = lines[idx];
        let trimmed = next.trim();
        if trimmed.is_empty() {
            if is_block_scalar || inline.is_empty() {
                idx += 1;
                continue;
            }
            break;
        }
        if next.starts_with(' ') || next.starts_with('\t') {
            let cleaned_raw = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim();
            let cleaned = if is_block_scalar {
                cleaned_raw
            } else {
                // A trailing backslash escapes the line break, not the next line's first character.
                quote_state.2 = false;
                let (comment_idx, next_state) = scan_yaml_quotes(cleaned_raw, quote_state, true);
                quote_state = next_state;
                match comment_idx {
                    Some(c) => cleaned_raw[..c].trim_end(),
                    None => cleaned_raw,
                }
            };
            if !cleaned.is_empty() {
                parts.push(cleaned.to_string());
            }
            idx += 1;
        } else {
            break;
        }
    }

    (parts.join(" "), idx)
}

/// Strips an inline `# ...` YAML comment outside single/double quotes when `#` is at
/// index 0 or preceded by ASCII whitespace, preserving `#` inside quoted scalars.
fn strip_yaml_inline_comment(raw: &str) -> &str {
    let (comment_idx, _) = scan_yaml_quotes(raw, (false, false, false), true);
    match comment_idx {
        Some(idx) => raw[..idx].trim_end(),
        None => raw,
    }
}

fn scan_yaml_quotes(
    raw: &str,
    (mut in_single, mut in_double, mut escaped): (bool, bool, bool),
    stop_at_comment: bool,
) -> (Option<usize>, (bool, bool, bool)) {
    let mut prev_is_ws = false;

    for (idx, ch) in raw.char_indices() {
        if in_double {
            if escaped {
                escaped = false;
                prev_is_ws = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                prev_is_ws = false;
                continue;
            }
            if ch == '"' {
                in_double = false;
            }
            prev_is_ws = ch.is_ascii_whitespace();
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            prev_is_ws = ch.is_ascii_whitespace();
            continue;
        }
        match ch {
            '"' => in_double = true,
            '\'' => in_single = true,
            '#' if stop_at_comment && (idx == 0 || prev_is_ws) => {
                return (Some(idx), (in_single, in_double, escaped));
            }
            _ => {}
        }
        prev_is_ws = ch.is_ascii_whitespace();
    }

    (None, (in_single, in_double, escaped))
}

fn unquote_yaml_scalar(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_empty_yaml_value(s: &str) -> bool {
    let u = unquote_yaml_scalar(s);
    u.is_empty() || u == "[]" || u == "null" || u == "~"
}

fn classify_rule_timing(fm_opt: Option<&Frontmatter<'_>>) -> LoadTiming {
    let Some(fm) = fm_opt else {
        return LoadTiming::Always;
    };
    if fm.always_apply == Some(false) {
        return LoadTiming::Deferred;
    }
    if fm.always_apply == Some(true) {
        return LoadTiming::Always;
    }
    if let Some(trigger) = fm
        .trigger
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return if trigger.eq_ignore_ascii_case("always_on")
            || trigger.eq_ignore_ascii_case("always")
        {
            LoadTiming::Always
        } else {
            LoadTiming::Deferred
        };
    }
    let has_conditional_trigger = fm.has_globs || fm.has_paths || fm.has_description;
    if has_conditional_trigger {
        LoadTiming::Deferred
    } else {
        LoadTiming::Always
    }
}

fn has_ascii_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

fn file_stem_or_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}
