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

    let default_lazy_tool_json =
        r#"{"name":"alpha_default_lazy","description":"Default lazy tool without eager flag"}"#;
    write_file(
        &mcp_srv1.join("alpha_default_lazy.json"),
        default_lazy_tool_json.as_bytes(),
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
        br#"{"mcpServers":{"global_srv":{"command":"bin","args":["--token","x"]},"srv_alpha":{"url":"http://localhost","tools":{"alpha_search":{"eager":true}}}}}"#,
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
        est_text_tokens(default_lazy_tool_json)
            + est_text_tokens(lazy_tool_json)
            + est_text_tokens(defer_tool_json)
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
        i.kind == CustomizationKind::McpTool
            && i.timing == LoadTiming::Always
            && i.name == "alpha_search"
            && i.source == "mcp:srv_alpha"
    }));
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::McpTool
            && i.timing == LoadTiming::Deferred
            && i.name == "alpha_default_lazy"
            && i.source == "mcp:srv_alpha"
    }));
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
        "DUMMY_API_KEY_VALUE_DO_NOT_LEAK",
        "DUMMY_TOKEN_VALUE_777",
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
                        }},
                        "tools": {{
                            "safe_tool": {{
                                "eager": true,
                                "token": "{}",
                                "headers": {{ "Authorization": "{}" }},
                                "args": ["--secret", "{}"]
                            }},
                            "bad_eager_type": {{
                                "eager": "{}",
                                "extra": ["{}", "{}"]
                            }}
                        }}
                    }}
                }}
            }}"#,
            secrets[5],
            secrets[2],
            secrets[0],
            secrets[4],
            secrets[1],
            secrets[3],
            secrets[3],
            secrets[1],
            secrets[5],
            secrets[0],
            secrets[2],
            secrets[4]
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

/// CUST-07: Inline YAML comments in frontmatter are stripped outside quotes while
/// preserving `#` inside single/double-quoted scalars and multi-line block scalars.
#[test]
fn cust_07_frontmatter_inline_comments_and_quoted_hashes() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    let plugin_dir = g.join("config").join("plugins").join("comment-plugin");
    let rules_dir = plugin_dir.join("rules");

    let deferred_comment_rule = "\
---
alwaysApply: false # 必要時のみ
---
# Deferred With Inline Comment
";
    write_file(
        &rules_dir.join("deferred_comment.md"),
        deferred_comment_rule.as_bytes(),
    );

    let always_comment_rule = "\
---
alwaysApply: true # 常時適用
globs: [] # empty list
paths:
  # only a comment in paths
---
# Always With Inline Comments
";
    write_file(
        &rules_dir.join("always_comment.md"),
        always_comment_rule.as_bytes(),
    );

    let block_scalar_rule = "\
---
description: | # block scalar header comment
  # Literal markdown heading inside block scalar
  Details here.
---
# Block Scalar Description Rule
";
    write_file(
        &rules_dir.join("block_scalar.md"),
        block_scalar_rule.as_bytes(),
    );

    let agents_dir = plugin_dir.join("agents");
    let rule_hash_agent = "\
---
name: \"Rule #1\" # trailing comment
description: \"Handles \\\" # escaped hash\" # outside comment
---
# Agent 1
";
    write_file(&agents_dir.join("agent1.md"), rule_hash_agent.as_bytes());

    let skill_dir = plugin_dir.join("skills").join("hash-skill");
    let skill_md = "\
---
name: 'Skill #2' # single-quoted hash preserved
description: 'Checks #2 behavior'
---
Skill body.
";
    write_file(&skill_dir.join("SKILL.md"), skill_md.as_bytes());

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert!(
        inv.scan_warnings.is_empty(),
        "unexpected warnings: {:?}",
        inv.scan_warnings
    );

    assert_eq!(
        inv.breakdown.rules_deferred,
        est_text_tokens(deferred_comment_rule) + est_text_tokens(block_scalar_rule)
    );
    assert_eq!(
        inv.breakdown.rules_files,
        est_text_tokens(always_comment_rule)
    );

    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::RuleFile
            && i.name == "deferred_comment"
            && i.timing == LoadTiming::Deferred
    }));
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::RuleFile
            && i.name == "always_comment"
            && i.timing == LoadTiming::Always
    }));
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::CustomAgent
            && i.name == "Rule #1"
            && i.timing == LoadTiming::Always
    }));
    assert!(inv.items.iter().any(|i| {
        i.kind == CustomizationKind::SkillIndex
            && i.name == "Skill #2"
            && i.timing == LoadTiming::Always
    }));

    // Multi-line quoted value with `#` followed by closing quote and trailing `#` comment,
    // plus a large multi-line `paths:` list (~8,000 entries under 512 KiB) to verify linear-time parsing.
    let multiline_agent = "\
---
name: \"Multi #1
  part #2\" # trailing comment after closed quote
---
Body.
";
    write_file(&agents_dir.join("multiline.md"), multiline_agent.as_bytes());

    let escaped_eol_agent = "\
---
name: \"Build\\
  \\\" #tag\" # trailing comment after closed quote
---
Body.
";
    write_file(
        &agents_dir.join("escaped_eol.md"),
        escaped_eol_agent.as_bytes(),
    );

    let mut large_paths_rule = String::from("---\npaths:\n");
    for i in 0..8000 {
        large_paths_rule.push_str(&format!(
            "  - crates/mod_{i:04}/some_reasonably_long_file.rs\n"
        ));
    }
    large_paths_rule.push_str("---\n# Large Paths Rule\n");
    assert!(large_paths_rule.len() < 512 * 1024);
    write_file(
        &rules_dir.join("large_paths.md"),
        large_paths_rule.as_bytes(),
    );

    let inv2 = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert!(inv2.scan_warnings.is_empty());
    assert!(inv2.items.iter().any(|i| {
        i.kind == CustomizationKind::CustomAgent
            && i.name == "Multi #1 part #2"
            && i.timing == LoadTiming::Always
    }));
    assert!(inv2.items.iter().any(|i| {
        i.kind == CustomizationKind::CustomAgent
            && i.name == "Build\\ \\\" #tag"
            && i.timing == LoadTiming::Always
    }));
    assert!(inv2.items.iter().any(|i| {
        i.kind == CustomizationKind::RuleFile
            && i.name == "large_paths"
            && i.timing == LoadTiming::Deferred
    }));
}

#[test]
fn cust_08_trigger_always_on_with_description_is_always() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();
    let rules_dir = g.join("config").join("plugins").join("p1").join("rules");

    let always_on_body = "\
---
trigger: always_on
description: Applied every turn even with description
---
Always on rule body.
";
    let always_short_body = "\
---
trigger: always
description: Short trigger alias with description
---
Always short rule body.
";
    let model_decision_body = "\
---
trigger: model_decision
description: Conditional rule triggered by model decision
---
Model decision rule body.
";
    let explicit_false_body = "\
---
alwaysApply: false
trigger: always_on
description: Explicit alwaysApply false wins over trigger
---
Explicit false rule body.
";

    write_file(
        &rules_dir.join("always_on_desc.md"),
        always_on_body.as_bytes(),
    );
    write_file(
        &rules_dir.join("always_desc.md"),
        always_short_body.as_bytes(),
    );
    write_file(
        &rules_dir.join("model_decision_desc.md"),
        model_decision_body.as_bytes(),
    );
    write_file(
        &rules_dir.join("explicit_false.md"),
        explicit_false_body.as_bytes(),
    );

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert!(inv.scan_warnings.is_empty());

    let find_rule = |name: &str| {
        inv.items
            .iter()
            .find(|i| i.kind == CustomizationKind::RuleFile && i.name == name)
            .unwrap_or_else(|| panic!("missing rule {name}"))
    };

    let r_always_on = find_rule("always_on_desc");
    assert_eq!(r_always_on.timing, LoadTiming::Always);

    let r_always = find_rule("always_desc");
    assert_eq!(r_always.timing, LoadTiming::Always);

    let r_model = find_rule("model_decision_desc");
    assert_eq!(r_model.timing, LoadTiming::Deferred);

    let r_false = find_rule("explicit_false");
    assert_eq!(r_false.timing, LoadTiming::Deferred);

    assert_eq!(
        inv.breakdown.rules_files,
        r_always_on.estimated_tokens + r_always.estimated_tokens
    );
    assert_eq!(
        sum_items(&inv, CustomizationKind::RuleFile, LoadTiming::Deferred),
        r_model.estimated_tokens + r_false.estimated_tokens
    );
}

/// CUST-09: Antigravity MCP tools default to Deferred (Lazy), and only tools configured with
/// `tools.<name>.eager: true` in matching `mcp_config.json` / `.mcp.json` become Always without
/// leaking across plugin prefixes.
#[test]
fn cust_09_mcp_default_lazy_and_plugin_prefixed_eager() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    // 1. Plugin `google-developer-knowledge` has NO `tools` eager entries -> all tools Deferred.
    let gdk_dir = g
        .join("antigravity")
        .join("mcp")
        .join("google-developer-knowledge_google-developer-knowledge");
    let gdk_search = r#"{"name":"search_documents","description":"Search developer docs"}"#;
    let gdk_get = r#"{"name":"get_documents","description":"Get developer docs"}"#;
    let gdk_answer = r#"{"name":"answer_query","description":"Answer developer query"}"#;
    write_file(
        &gdk_dir.join("search_documents.json"),
        gdk_search.as_bytes(),
    );
    write_file(&gdk_dir.join("get_documents.json"), gdk_get.as_bytes());
    write_file(&gdk_dir.join("answer_query.json"), gdk_answer.as_bytes());
    write_file(
        &g.join("config")
            .join("plugins")
            .join("google-developer-knowledge")
            .join("mcp_config.json"),
        br#"{"mcpServers":{"google-developer-knowledge":{"serverUrl":"https://example.invalid/mcp"}}}"#,
    );

    // 2. Plugin `gemini-api` enables `eager: true` on 2 tools and `eager: false` on 1 tool;
    //    an unlisted tool in the same server directory stays Deferred.
    let gemini_dir = g
        .join("antigravity")
        .join("mcp")
        .join("gemini-api_gemini-api-docs");
    let gem_search = r#"{"name":"gemini_search_docs","description":"Search Gemini API docs"}"#;
    let gem_get = r#"{"name":"gemini_get_doc","description":"Fetch Gemini API doc"}"#;
    let gem_opt_out = r#"{"name":"gemini_opt_out","description":"Explicitly eager: false"}"#;
    let gem_unlisted = r#"{"name":"gemini_unlisted","description":"Omitted from tools map"}"#;
    write_file(
        &gemini_dir.join("gemini_search_docs.json"),
        gem_search.as_bytes(),
    );
    write_file(&gemini_dir.join("gemini_get_doc.json"), gem_get.as_bytes());
    write_file(
        &gemini_dir.join("gemini_opt_out.json"),
        gem_opt_out.as_bytes(),
    );
    write_file(
        &gemini_dir.join("gemini_unlisted.json"),
        gem_unlisted.as_bytes(),
    );
    write_file(
        &g.join("config")
            .join("plugins")
            .join("gemini-api")
            .join("mcp_config.json"),
        br#"{
            "mcpServers": {
                "gemini-api-docs": {
                    "serverUrl": "https://example.invalid/gemini",
                    "tools": {
                        "gemini_search_docs": { "eager": true },
                        "gemini_get_doc": { "eager": true },
                        "gemini_opt_out": { "eager": false }
                    }
                }
            }
        }"#,
    );

    // 3. Same server key `gemini-api-docs` under another plugin and at root must NOT inherit
    //    `gemini-api` plugin's eager flags.
    let other_dir = g
        .join("antigravity")
        .join("mcp")
        .join("other-plugin_gemini-api-docs");
    let other_tool = r#"{"name":"gemini_search_docs","description":"Other plugin tool"}"#;
    write_file(
        &other_dir.join("gemini_search_docs.json"),
        other_tool.as_bytes(),
    );

    let root_same_srv = g.join("antigravity").join("mcp").join("gemini-api-docs");
    let root_same_tool = r#"{"name":"gemini_search_docs","description":"Root server tool"}"#;
    write_file(
        &root_same_srv.join("gemini_search_docs.json"),
        root_same_tool.as_bytes(),
    );

    // 4. Top-level `.mcp.json` with `$schema` scalar and `tools.<name>.eager: true`
    let top_plugin_dir = g.join("antigravity").join("mcp").join("top-plug_top_srv");
    let top_eager = r#"{"name":"top_eager_tool","description":"Top-level .mcp.json eager tool"}"#;
    let top_lazy = r#"{"name":"top_lazy_tool","description":"Top-level .mcp.json lazy tool"}"#;
    write_file(
        &top_plugin_dir.join("top_eager_tool.json"),
        top_eager.as_bytes(),
    );
    write_file(
        &top_plugin_dir.join("top_lazy_tool.json"),
        top_lazy.as_bytes(),
    );
    write_file(
        &g.join("config")
            .join("plugins")
            .join("top-plug")
            .join(".mcp.json"),
        br#"{
            "$schema": "https://example.invalid/schema.json",
            "top_srv": {
                "command": "node",
                "tools": {
                    "top_eager_tool": { "eager": true }
                }
            }
        }"#,
    );

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert!(
        inv.scan_warnings.is_empty(),
        "unexpected warnings: {:?}",
        inv.scan_warnings
    );

    let find_tool = |source: &str, name: &str| {
        inv.items
            .iter()
            .find(|i| i.kind == CustomizationKind::McpTool && i.source == source && i.name == name)
            .unwrap_or_else(|| panic!("missing tool {name} in {source}"))
    };

    assert_eq!(
        find_tool("mcp:gemini-api_gemini-api-docs", "gemini_search_docs").timing,
        LoadTiming::Always
    );
    assert_eq!(
        find_tool("mcp:gemini-api_gemini-api-docs", "gemini_get_doc").timing,
        LoadTiming::Always
    );
    assert_eq!(
        find_tool("mcp:gemini-api_gemini-api-docs", "gemini_opt_out").timing,
        LoadTiming::Deferred
    );
    assert_eq!(
        find_tool("mcp:gemini-api_gemini-api-docs", "gemini_unlisted").timing,
        LoadTiming::Deferred
    );

    for name in ["search_documents", "get_documents", "answer_query"] {
        assert_eq!(
            find_tool(
                "mcp:google-developer-knowledge_google-developer-knowledge",
                name
            )
            .timing,
            LoadTiming::Deferred
        );
    }

    assert_eq!(
        find_tool("mcp:other-plugin_gemini-api-docs", "gemini_search_docs").timing,
        LoadTiming::Deferred
    );
    assert_eq!(
        find_tool("mcp:gemini-api-docs", "gemini_search_docs").timing,
        LoadTiming::Deferred
    );
    assert_eq!(
        find_tool("mcp:top-plug_top_srv", "top_eager_tool").timing,
        LoadTiming::Always
    );
    assert_eq!(
        find_tool("mcp:top-plug_top_srv", "top_lazy_tool").timing,
        LoadTiming::Deferred
    );

    let expected_always =
        est_text_tokens(gem_search) + est_text_tokens(gem_get) + est_text_tokens(top_eager);
    let expected_deferred = est_text_tokens(gdk_search)
        + est_text_tokens(gdk_get)
        + est_text_tokens(gdk_answer)
        + est_text_tokens(gem_opt_out)
        + est_text_tokens(gem_unlisted)
        + est_text_tokens(other_tool)
        + est_text_tokens(root_same_tool)
        + est_text_tokens(top_lazy);

    assert_eq!(inv.breakdown.mcp_tools, expected_always);
    assert_eq!(inv.breakdown.mcp_deferred, expected_deferred);
    assert_eq!(
        sum_items(&inv, CustomizationKind::McpTool, LoadTiming::Always),
        expected_always
    );
    assert_eq!(
        sum_items(&inv, CustomizationKind::McpTool, LoadTiming::Deferred),
        expected_deferred
    );
}

/// CUST-10: Precedence between plugin / config / antigravity MCP configs, legacy
/// `lazy`/`defer` compatibility, and `instructions.md` remaining Always.
#[test]
fn cust_10_mcp_eager_precedence_and_legacy_lazy_compat() {
    let tmp = TempDir::new().unwrap();
    let g = tmp.path();

    let plug_srv = g.join("antigravity").join("mcp").join("my-plug_srv");
    let instr_body = "Server instructions stay Always even if all tools are deferred.";
    write_file(&plug_srv.join("instructions.md"), instr_body.as_bytes());

    let overridden_off = r#"{"name":"overridden_off","description":"Plugin eager:true overridden to false by antigravity root"}"#;
    let overridden_on = r#"{"name":"overridden_on","description":"Config root eager:false overridden to true by antigravity root"}"#;
    let legacy_lazy = r#"{"name":"legacy_lazy","lazy":true,"description":"Legacy lazy stays Deferred despite eager:true"}"#;
    let legacy_defer = r#"{"name":"legacy_defer","defer":true,"description":"Legacy defer stays Deferred despite eager:true"}"#;
    let stem_fallback = r#"{"description":"Tool without JSON name matched by file stem"}"#;

    write_file(
        &plug_srv.join("overridden_off.json"),
        overridden_off.as_bytes(),
    );
    write_file(
        &plug_srv.join("overridden_on.json"),
        overridden_on.as_bytes(),
    );
    write_file(&plug_srv.join("legacy_lazy.json"), legacy_lazy.as_bytes());
    write_file(&plug_srv.join("legacy_defer.json"), legacy_defer.as_bytes());
    write_file(
        &plug_srv.join("stem_fallback.json"),
        stem_fallback.as_bytes(),
    );

    // Precedence 0 (Plugin): sets overridden_off=true, legacy_lazy=true, legacy_defer=true, stem_fallback=true
    write_file(
        &g.join("config")
            .join("plugins")
            .join("my-plug")
            .join("mcp_config.json"),
        br#"{
            "mcpServers": {
                "srv": {
                    "tools": {
                        "overridden_off": { "eager": true },
                        "legacy_lazy": { "eager": true },
                        "legacy_defer": { "eager": true },
                        "stem_fallback": { "eager": true }
                    }
                }
            }
        }"#,
    );

    // Precedence 1 (config/mcp_config.json): sets overridden_on=false
    write_file(
        &g.join("config").join("mcp_config.json"),
        br#"{
            "mcpServers": {
                "my-plug_srv": {
                    "tools": {
                        "overridden_on": { "eager": false }
                    }
                }
            }
        }"#,
    );

    // Precedence 2 (antigravity/mcp_config.json): overrides overridden_off=false and overridden_on=true
    write_file(
        &g.join("antigravity").join("mcp_config.json"),
        br#"{
            "mcpServers": {
                "my-plug_srv": {
                    "tools": {
                        "overridden_off": { "eager": false },
                        "overridden_on": { "eager": true }
                    }
                }
            }
        }"#,
    );

    let inv = scan_customizations(&GeminiPaths::from_gemini_dir(g));
    assert!(
        inv.scan_warnings.is_empty(),
        "unexpected warnings: {:?}",
        inv.scan_warnings
    );

    let find_tool = |name: &str| {
        inv.items
            .iter()
            .find(|i| {
                i.kind == CustomizationKind::McpTool
                    && i.source == "mcp:my-plug_srv"
                    && i.name == name
            })
            .unwrap_or_else(|| panic!("missing tool {name}"))
    };

    assert_eq!(find_tool("overridden_off").timing, LoadTiming::Deferred);
    assert_eq!(find_tool("overridden_on").timing, LoadTiming::Always);
    assert_eq!(find_tool("legacy_lazy").timing, LoadTiming::Deferred);
    assert_eq!(find_tool("legacy_defer").timing, LoadTiming::Deferred);
    assert_eq!(find_tool("stem_fallback").timing, LoadTiming::Always);

    assert_eq!(inv.breakdown.mcp_instructions, est_text_tokens(instr_body));
    assert_eq!(
        inv.breakdown.mcp_tools,
        est_text_tokens(overridden_on) + est_text_tokens(stem_fallback)
    );
    assert_eq!(
        inv.breakdown.mcp_deferred,
        est_text_tokens(overridden_off)
            + est_text_tokens(legacy_lazy)
            + est_text_tokens(legacy_defer)
    );
}
