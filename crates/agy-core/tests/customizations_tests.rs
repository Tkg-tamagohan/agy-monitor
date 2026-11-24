use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use agy_core::analyzer::est_text_tokens;
use agy_core::customizations::{
    CustomizationInventory, CustomizationKind, LoadTiming, MAX_FILE_BYTES, MAX_SCAN_DEPTH,
    MAX_SCAN_FILES, MAX_WARNINGS, scan_customizations, scan_customizations_cancellable,
};
use agy_core::paths::GeminiPaths;
use tempfile::TempDir;

fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn sum_items(inv: &CustomizationInventory, kind: CustomizationKind, timing: LoadTiming) -> u64 {
    inv.items
        .iter()
        .filter(|i| i.kind == kind && i.timing == timing)
        .map(|i| i.estimated_tokens)
        .sum()
}

/// CUST-01: Full mock `.gemini` tree covers MCP eager/deferred tools, MCP instructions,
/// builtin & plugin skills, always & deferred rules, custom agents, counts, sorting, and breakdown sums.
#[test]
fn cust_01_full_mock_tree_and_breakdown_sums() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    // 1. antigravity/mcp/<server>/
    let mcp_srv1 = g.join("antigravity").join("mcp").join("srv_alpha");
    let instr_text = "Always validate parameters before calling srv_alpha tools.";
    write_file(&mcp_srv1.join("instructions.md"), instr_text.as_bytes());

    let eager_tool_json =
        r#"{"name":"alpha_search","description":"Search index","parameters":{"type":"object"}}"#;
    write_file(
        &mcp_srv1.join("alpha_search.json"),
        eager_tool_json.as_bytes(),
    );

    let lazy_tool_json = r#"{"name":"alpha_heavy","lazy":true,"description":"Heavy operation with long schema details for testing"}"#;
    write_file(
        &mcp_srv1.join("alpha_heavy.json"),
        lazy_tool_json.as_bytes(),
    );

    let defer_tool_json =
        r#"{"name":"alpha_defer","defer":true,"description":"Deferred via defer flag"}"#;
    write_file(
        &mcp_srv1.join("alpha_defer.json"),
        defer_tool_json.as_bytes(),
    );

    // 2. antigravity/builtin/skills/*/SKILL.md
    let builtin_skill = g
        .join("antigravity")
        .join("builtin")
        .join("skills")
        .join("code-review");
    let builtin_skill_md = "\
---
name: code-review
description: Reviews code changes for correctness and edge cases.
---

# Detailed Skill Body

Step 1: Inspect all diffs carefully.
Step 2: Verify invariants and unit test coverage across modules.
";
    write_file(&builtin_skill.join("SKILL.md"), builtin_skill_md.as_bytes());

    // 3. config/plugins/<plugin>/
    let p1 = g.join("config").join("plugins").join("plugin-one");
    let rule_always_plain = "# Plain Always Rule\nAlways preserve docstrings.\n";
    write_file(
        &p1.join("rules").join("plain_always.md"),
        rule_always_plain.as_bytes(),
    );

    let rule_always_explicit = "\
---
alwaysApply: true
description: Has description but alwaysApply is true so it stays Always.
---
# Explicit Always
Follow strict conventions.
";
    write_file(
        &p1.join("rules").join("explicit_always.md"),
        rule_always_explicit.as_bytes(),
    );

    let rule_deferred_flag = "\
---
alwaysApply: false
---
# Disabled Always Apply
Load only on demand.
";
    write_file(
        &p1.join("rules").join("deferred_flag.md"),
        rule_deferred_flag.as_bytes(),
    );

    let rule_deferred_glob = "\
---
trigger: glob
globs: \"crates/**/*.rs\"
description: Rust-specific guidelines.
---
# Rust Guidelines
Use Result properly.
";
    write_file(
        &p1.join("rules").join("deferred_glob.md"),
        rule_deferred_glob.as_bytes(),
    );

    let rule_deferred_paths = "\
---
paths:
  - \"src/**\"
---
# Path Triggered Rule
";
    write_file(
        &p1.join("rules").join("deferred_paths.md"),
        rule_deferred_paths.as_bytes(),
    );

    let plugin_skill_md = "\
---
name: deploy-check
description: Checks deployment readiness.
---
Full checklist body for deployment verification.
";
    write_file(
        &p1.join("skills").join("deploy-check").join("SKILL.md"),
        plugin_skill_md.as_bytes(),
    );

    let agent_md = "\
---
name: security_auditor
description: Audits security boundaries.
---
# Security Auditor
Check auth and secrets.
";
    write_file(&p1.join("agents").join("auditor.md"), agent_md.as_bytes());

    let subagent_md = "# Helper Subagent\nHandles parallel sub-tasks.\n";
    write_file(
        &p1.join("subagents").join("helper.md"),
        subagent_md.as_bytes(),
    );

    write_file(
        &p1.join(".mcp.json"),
        br#"{"mcpServers":{"plugin_one_srv":{"command":"node","env":{"API_KEY":"ignored"}}}}"#,
    );
    fs::create_dir_all(p1.join("mcp").join("plugin_dir_srv")).unwrap();

    // Second plugin (empty except README, still counts toward plugin_count)
    let p2 = g.join("config").join("plugins").join("plugin-two");
    write_file(&p2.join("README.md"), b"# Plugin Two\n");

    // 4. config/mcp_config.json & antigravity/mcp_config.json (shared key deduplicates)
    write_file(
        &g.join("config").join("mcp_config.json"),
        br#"{"mcpServers":{"global_srv":{"command":"bin","args":["--token","x"]},"srv_alpha":{"url":"http://localhost"}}}"#,
    );
    write_file(
        &g.join("antigravity").join("mcp_config.json"),
        br#"{"mcpServers":{"global_srv":{"url":"http://localhost"},"agy_only_srv":{"command":"agy"}}}"#,
    );

    let paths = GeminiPaths::from_gemini_dir(g);
    let inv = scan_customizations(&paths);

    assert!(
        inv.scan_warnings.is_empty(),
        "unexpected warnings: {:?}",
        inv.scan_warnings
    );
    assert_eq!(inv.plugin_count, 2);
    // Unique MCP servers: srv_alpha, plugin_one_srv, plugin_dir_srv, global_srv, agy_only_srv = 5
    assert_eq!(inv.mcp_server_count, 5);

    // Verify exact correspondence between grouped items and breakdown
    assert_eq!(
        inv.breakdown.mcp_tools,
        sum_items(&inv, CustomizationKind::McpTool, LoadTiming::Always)
    );
    assert_eq!(
        inv.breakdown.mcp_deferred,
        sum_items(&inv, CustomizationKind::McpTool, LoadTiming::Deferred)
    );
    assert_eq!(
        inv.breakdown.mcp_instructions,
        sum_items(&inv, CustomizationKind::McpInstruction, LoadTiming::Always)
    );
    assert_eq!(
        inv.breakdown.rules_files,
        sum_items(&inv, CustomizationKind::RuleFile, LoadTiming::Always)
    );
    assert_eq!(
        inv.breakdown.rules_deferred,
        sum_items(&inv, CustomizationKind::RuleFile, LoadTiming::Deferred)
    );
    assert_eq!(
        inv.breakdown.skills_index,
        sum_items(&inv, CustomizationKind::SkillIndex, LoadTiming::Always)
    );
    assert_eq!(
        inv.breakdown.skills_deferred,
        sum_items(&inv, CustomizationKind::SkillIndex, LoadTiming::Deferred)
    );
    assert_eq!(
        inv.breakdown.custom_agents,
        sum_items(&inv, CustomizationKind::CustomAgent, LoadTiming::Always)
    );

    // Verify expected item counts & classifications
    assert_eq!(inv.breakdown.mcp_instructions, est_text_tokens(instr_text));
    assert_eq!(inv.breakdown.mcp_tools, est_text_tokens(eager_tool_json));
    assert_eq!(
        inv.breakdown.mcp_deferred,
        est_text_tokens(lazy_tool_json) + est_text_tokens(defer_tool_json)
    );
    assert_eq!(
        inv.breakdown.rules_files,
        est_text_tokens(rule_always_plain) + est_text_tokens(rule_always_explicit)
    );
    assert_eq!(
        inv.breakdown.rules_deferred,
        est_text_tokens(rule_deferred_flag)
            + est_text_tokens(rule_deferred_glob)
            + est_text_tokens(rule_deferred_paths)
    );
    assert_eq!(
        inv.breakdown.custom_agents,
        est_text_tokens(agent_md) + est_text_tokens(subagent_md)
    );
    assert!(inv.breakdown.skills_index > 0);
    assert!(inv.breakdown.skills_deferred > inv.breakdown.skills_index);

    // Verify specific item fields (not just aggregates)
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::CustomAgent
            && i.timing == LoadTiming::Always
            && i.name == "security_auditor"
            && i.source == "plugin:plugin-one"
    }));
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::SkillIndex
            && i.timing == LoadTiming::Always
            && i.name == "code-review"
            && i.source == "builtin"
    }));
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::RuleFile
            && i.timing == LoadTiming::Deferred
            && i.name == "deferred_glob"
            && i.source == "plugin:plugin-one"
    }));

    // Verify deterministic sort: descending estimated_tokens, tie-break name, source
    for pair in inv.items.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        let ord = b
            .estimated_tokens
            .cmp(&a.estimated_tokens)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.path.cmp(&b.path));
        assert!(
            ord.is_le(),
            "items not sorted deterministically: {a:?} before {b:?}"
        );
    }
}

/// CUST-02: CRITICAL SECURITY INVARIANT - secrets in mcp_config.json, plugin mcp configs,
/// and config.json (both valid and malformed JSON) must NEVER appear anywhere in Debug output.
#[test]
fn cust_02_secrets_never_appear_in_inventory_or_warnings() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    let secrets = [
        "SUPER_SECRET_ENV_VALUE_XYZ123",
        "Bearer_TOP_SECRET_AUTH_HEADER_999",
        "AIzaSyFAKE_API_KEY_DO_NOT_LEAK",
        "ghp_SECRET_TOKEN_VALUE_777",
        "hunter2_SECRET_PASSWORD_000",
        "RAW_JSON_SNIPPET_MARKER_DO_NOT_APPEAR",
    ];

    // Valid config/mcp_config.json with sensitive env/headers/args
    write_file(
        &g.join("config").join("mcp_config.json"),
        format!(
            r#"{{
                "mcpServers": {{
                    "secure_srv": {{
                        "command": "node",
                        "args": ["--marker", "{}"],
                        "env": {{
                            "API_KEY": "{}",
                            "SECRET": "{}",
                            "PASSWORD": "{}"
                        }},
                        "headers": {{
                            "Authorization": "{}",
                            "X-Token": "{}"
                        }}
                    }}
                }}
            }}"#,
            secrets[5], secrets[2], secrets[0], secrets[4], secrets[1], secrets[3]
        )
        .as_bytes(),
    );

    // config/config.json with secrets (must never be read or retained)
    write_file(
        &g.join("config").join("config.json"),
        format!(
            r#"{{"token":"{}","apiKey":"{}","password":"{}","authorization":"{}"}}"#,
            secrets[3], secrets[2], secrets[4], secrets[1]
        )
        .as_bytes(),
    );

    // Malformed antigravity/mcp_config.json embedding a secret string directly
    write_file(
        &g.join("antigravity").join("mcp_config.json"),
        format!(r#"{{"mcpServers": "{}"}}"#, secrets[0]).as_bytes(),
    );

    // Plugin malformed .mcp.json embedding a secret string
    write_file(
        &g.join("config")
            .join("plugins")
            .join("sec-plugin")
            .join(".mcp.json"),
        format!(r#"{{"env":{{"token":"{}"}} invalid_syntax"#, secrets[3]).as_bytes(),
    );

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert_eq!(inv.mcp_server_count, 1);
    assert_eq!(inv.scan_warnings.len(), 2);

    let debug_dump = format!("{inv:?}");
    for secret in secrets {
        assert!(
            !debug_dump.contains(secret),
            "secret `{secret}` leaked into inventory debug output: {debug_dump}"
        );
    }
    assert!(
        inv.scan_warnings
            .iter()
            .any(|w| w.contains("antigravity/mcp_config.json") && w.contains("invalid JSON"))
    );
    assert!(
        inv.scan_warnings.iter().any(
            |w| w.contains("config/plugins/sec-plugin/.mcp.json") && w.contains("invalid JSON")
        )
    );
}

/// CUST-03: Malformed tool JSON, oversized file (> MAX_FILE_BYTES), and non-UTF-8 binary files
/// are safely skipped with bounded warnings.
#[test]
fn cust_03_malformed_json_oversized_and_non_utf8_warnings() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    let srv = g.join("antigravity").join("mcp").join("broken_srv");
    write_file(&srv.join("broken_tool.json"), b"{not valid json");
    write_file(
        &srv.join("valid_tool.json"),
        br#"{"name":"ok_tool","description":"works"}"#,
    );

    let plugin_rules = g
        .join("config")
        .join("plugins")
        .join("heavy-plugin")
        .join("rules");

    // Oversized rule (> 512 KiB)
    let oversized = vec![b'a'; (MAX_FILE_BYTES as usize) + 64];
    write_file(&plugin_rules.join("oversized.md"), &oversized);

    // Non-UTF-8 binary rule
    write_file(
        &plugin_rules.join("binary.md"),
        &[0xff, 0xfe, 0x00, 0x80, 0xc3, 0x28],
    );

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert_eq!(inv.items.len(), 1);
    assert_eq!(inv.items[0].name, "ok_tool");
    assert_eq!(inv.scan_warnings.len(), 3);
    assert!(
        inv.scan_warnings
            .iter()
            .any(|w| w.contains("broken_tool.json") && w.contains("invalid JSON"))
    );
    assert!(
        inv.scan_warnings
            .iter()
            .any(|w| w.contains("oversized.md") && w.contains("oversized"))
    );
    assert!(
        inv.scan_warnings
            .iter()
            .any(|w| w.contains("binary.md") && w.contains("non-UTF-8"))
    );
}

/// CUST-04: AtomicBool cancellation returns None immediately when set.
#[test]
fn cust_04_cancellation_returns_none() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();
    write_file(
        &g.join("antigravity")
            .join("mcp")
            .join("srv")
            .join("tool.json"),
        br#"{"name":"t"}"#,
    );

    let paths = GeminiPaths::from_gemini_dir(g);

    let cancel_true = AtomicBool::new(true);
    assert!(scan_customizations_cancellable(&paths, &cancel_true).is_none());

    let cancel_false = AtomicBool::new(false);
    let some_inv = scan_customizations_cancellable(&paths, &cancel_false);
    assert!(some_inv.is_some());
    assert_eq!(some_inv.unwrap().items.len(), 1);
}

/// CUST-05: Bounded scanning enforces MAX_WARNINGS, MAX_SCAN_DEPTH, and MAX_SCAN_FILES.
#[test]
fn cust_05_scan_bounds_enforced() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    // Exceed MAX_WARNINGS with 60 malformed JSON files
    let srv = g.join("antigravity").join("mcp").join("noisy_srv");
    for i in 0..60 {
        write_file(&srv.join(format!("bad_{i:02}.json")), b"{");
    }

    // Exceed MAX_SCAN_DEPTH inside rules
    let mut deep_dir = g
        .join("config")
        .join("plugins")
        .join("deep-plugin")
        .join("rules");
    for level in 0..(MAX_SCAN_DEPTH + 2) {
        deep_dir = deep_dir.join(format!("d{level}"));
    }
    write_file(&deep_dir.join("too_deep.md"), b"# Deep Rule\n");

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert!(inv.scan_warnings.len() <= MAX_WARNINGS);
    assert_eq!(inv.scan_warnings.len(), MAX_WARNINGS);
    assert!(!inv.items.iter().any(|i| i.name == "too_deep"));

    // Enforce MAX_SCAN_FILES (create 2050 small valid tool JSONs)
    let tmp_many = TempDir::new().unwrap();
    let srv_many = tmp_many
        .path()
        .join("antigravity")
        .join("mcp")
        .join("many_srv");
    fs::create_dir_all(&srv_many).unwrap();
    for i in 0..(MAX_SCAN_FILES + 50) {
        let name = format!("t_{i:04}.json");
        fs::write(srv_many.join(name), br#"{"name":"x"}"#).unwrap();
    }
    let inv_many = scan_customizations(&GeminiPaths::from_gemini_dir(tmp_many.path()));
    assert!(inv_many.items.len() <= MAX_SCAN_FILES);
    assert!(
        inv_many
            .scan_warnings
            .iter()
            .any(|w| w.contains("scan file limit reached"))
    );
}

/// CUST-06: Missing `.gemini` root or empty tree returns empty inventory without warnings.
#[test]
fn cust_06_missing_dir_returns_empty_inventory() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does_not_exist");
    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(&missing));
    assert_eq!(inv, CustomizationInventory::default());
}
