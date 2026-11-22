use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::fingerprint::TranscriptFingerprint;
use crate::models::{ModelInfo, detect_conversation_model};
use crate::sessions::is_valid_conv_id;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallJson {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscriptStep {
    #[serde(default)]
    pub step_index: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(rename = "type", default)]
    pub step_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub media: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallJson>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPoint {
    pub step_index: u64,
    pub prompt_tokens: u64,
    pub uncached_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub created_at: Option<String>,
    pub compacted: bool,
}

impl TurnPoint {
    pub fn cache_hit_percent(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            (self.cached_tokens as f64 / self.prompt_tokens as f64) * 100.0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionEvent {
    pub step_index: u64,
    pub before_tokens: u64,
    pub after_tokens: Option<u64>,
    pub saved_tokens: u64,
    pub summary_tokens: u64,
    pub created_at: Option<String>,
    pub is_explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeavyToolCall {
    pub step_index: u64,
    pub tool_name: String,
    pub label: String,
    pub target_path: Option<String>,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSummary {
    pub step_index: u64,
    pub step_type: String,
    pub source: String,
    pub status: String,
    pub created_at: Option<String>,
    pub prompt_tokens: u64,
    pub uncached_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub est_tokens: u64,
    pub compacted: bool,
    pub summary: String,
}

impl StepSummary {
    pub fn cache_hit_percent(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            (self.cached_tokens as f64 / self.prompt_tokens as f64) * 100.0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedSubagent {
    pub type_name: String,
    pub role: String,
    pub model: String,
    pub conversation_id: Option<String>,
    pub step_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CustomizationBreakdown {
    pub mcp_tools: u64,
    pub mcp_instructions: u64,
    pub rules_files: u64,
    pub skills_index: u64,
    pub custom_agents: u64,
    pub mcp_deferred: u64,
    pub skills_deferred: u64,
    pub rules_deferred: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextBuckets {
    pub user_messages: u64,
    pub agent_replies: u64,
    pub agent_thinking: u64,
    pub tool_call_args: u64,
    pub tool_call_outputs: u64,
    pub system_tools: u64,
    pub system_prompt: u64,
    pub system_messages: u64,
    pub checkpoint_buffer: u64,
    pub free_space: u64,
    /// Reserved for Phase 2 customization/MCP breakdown. Always `None` in Phase 1.
    pub customization_breakdown: Option<CustomizationBreakdown>,
}

impl ContextBuckets {
    pub fn active_total(&self) -> u64 {
        let custom_sum = self
            .customization_breakdown
            .as_ref()
            .map(|c| {
                c.mcp_tools + c.mcp_instructions + c.rules_files + c.skills_index + c.custom_agents
            })
            .unwrap_or(0);
        self.user_messages
            + self.agent_replies
            + self.agent_thinking
            + self.tool_call_args
            + self.tool_call_outputs
            + self.system_tools
            + self.system_prompt
            + self.system_messages
            + self.checkpoint_buffer
            + custom_sum
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheStats {
    pub last_prompt_tokens: u64,
    pub since_last_cache_tokens: u64,
    pub cached_tokens: u64,
    pub cache_hit_ratio: f64,
    pub last_output_tokens: u64,
    pub cumulative_input_tokens: u64,
    pub cumulative_cached_tokens: u64,
    pub session_cache_hit_ratio: f64,
    pub cumulative_output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationAnalysis {
    pub conversation_id: String,
    pub found: bool,
    pub exact: bool,
    pub model: ModelInfo,
    pub context_limit: u64,
    pub used_tokens: u64,
    pub free_tokens: u64,
    pub used_percent: f64,
    pub step_count: usize,
    pub planner_turn_count: usize,
    pub last_step_at: Option<String>,
    pub cache: CacheStats,
    pub compactions: Vec<CompactionEvent>,
    pub total_saved_tokens: u64,
    pub buckets: ContextBuckets,
    pub turns_history: Vec<TurnPoint>,
    pub heavy_tool_calls: Vec<HeavyToolCall>,
    pub total_tool_calls: usize,
    pub step_summaries: Vec<StepSummary>,
    pub spawned_subagents: Vec<SpawnedSubagent>,
    pub db_busy: bool,
}

/// Estimates token count based on punctuation density (JSON/code vs natural prose).
/// Strictly UTF-8 character-boundary safe.
pub fn est_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let char_len = text.chars().count();
    if char_len == 0 {
        return 0;
    }

    let mut sample_len: usize = 0;
    let mut punct: usize = 0;
    for ch in text.chars().take(2000) {
        sample_len += 1;
        if matches!(ch, '{' | '}' | '[' | ']' | ':' | '"' | '<' | '>' | '\\') {
            punct += 1;
        }
    }

    let density = if sample_len > 0 {
        punct as f64 / sample_len as f64
    } else {
        0.0
    };
    let chars_per_token = if density > 0.08 { 3.45 } else { 3.85 };
    ((char_len as f64) / chars_per_token).ceil().max(1.0) as u64
}

pub fn est_step_tokens(step: &TranscriptStep) -> u64 {
    let mut tok = 0;
    if let Some(content) = step.content.as_deref()
        && !content.is_empty()
    {
        tok += est_text_tokens(content);
    }
    if let Some(media) = step.media.as_ref() {
        tok += (media.len() as u64) * 258;
    }
    tok + 6
}

pub fn parse_transcript_steps(raw_text: &str) -> Vec<TranscriptStep> {
    let mut steps = Vec::new();
    for line in raw_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(step) = serde_json::from_str::<TranscriptStep>(line) {
            steps.push(step);
        }
        // Torn trailing or malformed JSONL line is safely skipped.
    }
    steps
}

fn split_planner_output(step: &TranscriptStep, total_out: u64) -> (u64, u64, u64) {
    if total_out == 0 {
        return (0, 0, 0);
    }
    let reply_chars = step
        .content
        .as_deref()
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let think_chars = step
        .thinking
        .as_deref()
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let tc_chars = match step.tool_calls.as_ref() {
        Some(tcs) if !tcs.is_empty() => serde_json::to_string(tcs)
            .map(|s| s.chars().count())
            .unwrap_or(0),
        _ => 0,
    };
    let sum = reply_chars + think_chars + tc_chars;
    if sum == 0 {
        return (total_out, 0, 0);
    }
    let thinking = ((total_out as f64 * think_chars as f64) / sum as f64).round() as u64;
    let tool_args = ((total_out as f64 * tc_chars as f64) / sum as f64).round() as u64;
    let replies = total_out.saturating_sub(thinking + tool_args);
    (replies, thinking, tool_args)
}

pub fn reconcile_active_buckets(buckets: &mut ContextBuckets, target_total: u64) {
    let current = buckets.active_total();
    if target_total == current {
        return;
    }
    if target_total > current {
        buckets.system_prompt += target_total - current;
        return;
    }

    let mut remaining = current - target_total;
    let fields: [&mut u64; 9] = [
        &mut buckets.agent_thinking,
        &mut buckets.tool_call_outputs,
        &mut buckets.agent_replies,
        &mut buckets.system_prompt,
        &mut buckets.user_messages,
        &mut buckets.tool_call_args,
        &mut buckets.system_tools,
        &mut buckets.system_messages,
        &mut buckets.checkpoint_buffer,
    ];

    for slot in fields {
        if remaining == 0 {
            break;
        }
        let take = (*slot).min(remaining);
        *slot -= take;
        remaining -= take;
    }
}

pub fn analyze_conversation(
    app_data: &Path,
    conv_id: &str,
    explicit_context_limit: Option<u64>,
    pbtxt_override: Option<&Path>,
) -> std::result::Result<ConversationAnalysis, CoreError> {
    analyze_conversation_with_meta(app_data, conv_id, explicit_context_limit, pbtxt_override)
        .map(|(analysis, _)| analysis)
}

pub(crate) fn analyze_conversation_with_meta(
    app_data: &Path,
    conv_id: &str,
    explicit_context_limit: Option<u64>,
    pbtxt_override: Option<&Path>,
) -> std::result::Result<(ConversationAnalysis, bool), CoreError> {
    if !is_valid_conv_id(conv_id) {
        return Err(CoreError::InvalidConversationId(conv_id.to_string()));
    }

    let model_res = detect_conversation_model(app_data, conv_id, pbtxt_override);
    let model_from_db = model_res.model_from_db;
    let model_info = model_res.info;
    let context_limit = explicit_context_limit
        .filter(|&v| v > 0)
        .unwrap_or(model_info.context_limit);

    let conv_brain_dir = app_data.join("brain").join(conv_id);
    let logs_dir = conv_brain_dir.join(".system_generated").join("logs");
    let full_path = logs_dir.join("transcript_full.jsonl");
    let compact_path = logs_dir.join("transcript.jsonl");
    let tfp = TranscriptFingerprint::from_logs_dir(&logs_dir);

    let steps = load_combined_steps(&full_path, &compact_path, &tfp);

    if !tfp.exists() && !conv_brain_dir.exists() {
        return Ok((
            ConversationAnalysis {
                conversation_id: conv_id.to_string(),
                found: false,
                exact: false,
                model: model_info,
                context_limit,
                used_tokens: 0,
                free_tokens: context_limit,
                used_percent: 0.0,
                step_count: 0,
                planner_turn_count: 0,
                last_step_at: None,
                cache: CacheStats {
                    last_prompt_tokens: 0,
                    since_last_cache_tokens: 0,
                    cached_tokens: 0,
                    cache_hit_ratio: 0.0,
                    last_output_tokens: 0,
                    cumulative_input_tokens: 0,
                    cumulative_cached_tokens: 0,
                    session_cache_hit_ratio: 0.0,
                    cumulative_output_tokens: 0,
                },
                compactions: Vec::new(),
                total_saved_tokens: 0,
                buckets: ContextBuckets {
                    free_space: context_limit,
                    ..Default::default()
                },
                turns_history: Vec::new(),
                heavy_tool_calls: Vec::new(),
                total_tool_calls: 0,
                step_summaries: Vec::new(),
                spawned_subagents: Vec::new(),
                db_busy: model_res.db_busy,
            },
            model_from_db,
        ));
    }

    let mut planner_indices: Vec<usize> = Vec::new();
    let mut compactions: Vec<CompactionEvent> = Vec::new();
    let mut turns_history: Vec<TurnPoint> = Vec::new();
    let mut compacted_step_positions: HashSet<usize> = HashSet::new();
    let mut last_compaction_step_idx: Option<usize> = None;
    let mut last_checkpoint_summary_tokens: u64 = 0;
    let mut saw_explicit_checkpoint_since_last_planner = false;
    let mut last_planner_idx_so_far: Option<usize> = None;

    for (i, s) in steps.iter().enumerate() {
        let stype = s.step_type.as_deref().unwrap_or("");
        if stype == "CHECKPOINT" {
            compacted_step_positions.insert(i);
            let cp_prompt = s.input_tokens.unwrap_or(0) + s.cache_read_tokens.unwrap_or(0);
            let prev_planner_total = last_planner_idx_so_far
                .map(|pi| {
                    let ps = &steps[pi];
                    ps.input_tokens.unwrap_or(0)
                        + ps.cache_read_tokens.unwrap_or(0)
                        + ps.output_tokens.unwrap_or(0)
                })
                .unwrap_or(0);
            let before_tokens = if cp_prompt > 0 {
                cp_prompt
            } else {
                prev_planner_total
            };
            let summary_tokens = match s.output_tokens {
                Some(out) if out > 0 => out,
                _ => est_step_tokens(s),
            };

            let mut after_tokens = None;
            for next_step in steps.iter().skip(i + 1) {
                let ntype = next_step.step_type.as_deref().unwrap_or("");
                if ntype == "CHECKPOINT" {
                    break;
                }
                if ntype == "PLANNER_RESPONSE" {
                    let np = next_step.input_tokens.unwrap_or(0)
                        + next_step.cache_read_tokens.unwrap_or(0);
                    if np > 0 {
                        after_tokens = Some(np);
                        break;
                    }
                }
            }

            let saved_tokens = after_tokens
                .map(|a| before_tokens.saturating_sub(a))
                .unwrap_or(0);

            compactions.push(CompactionEvent {
                step_index: s.step_index.unwrap_or(i as u64),
                before_tokens,
                after_tokens,
                saved_tokens,
                summary_tokens,
                created_at: s.created_at.clone(),
                is_explicit: true,
            });
            last_compaction_step_idx = Some(i);
            last_checkpoint_summary_tokens = summary_tokens;
            saw_explicit_checkpoint_since_last_planner = true;
        } else if stype == "PLANNER_RESPONSE" {
            let inp = s.input_tokens.unwrap_or(0);
            let cache = s.cache_read_tokens.unwrap_or(0);
            let prompt_tokens = inp + cache;
            let output_tokens = s.output_tokens.unwrap_or(0);
            if prompt_tokens > 0 {
                let mut implicit_drop = false;
                if let Some(&prev_idx) = planner_indices.last()
                    && !saw_explicit_checkpoint_since_last_planner
                {
                    let prev_prompt = steps[prev_idx].input_tokens.unwrap_or(0)
                        + steps[prev_idx].cache_read_tokens.unwrap_or(0);
                    if prev_prompt > prompt_tokens {
                        let drop = prev_prompt - prompt_tokens;
                        let threshold = 5000.0_f64.max(prev_prompt as f64 * 0.12);
                        if (drop as f64) > threshold {
                            implicit_drop = true;
                            compacted_step_positions.insert(i);
                            last_compaction_step_idx = Some(i.saturating_sub(1));
                            last_checkpoint_summary_tokens = 0;
                            compactions.push(CompactionEvent {
                                step_index: s.step_index.unwrap_or(i as u64),
                                before_tokens: prev_prompt,
                                after_tokens: Some(prompt_tokens),
                                saved_tokens: drop,
                                summary_tokens: 0,
                                created_at: s.created_at.clone(),
                                is_explicit: false,
                            });
                        }
                    }
                }
                planner_indices.push(i);
                last_planner_idx_so_far = Some(i);
                turns_history.push(TurnPoint {
                    step_index: s.step_index.unwrap_or(i as u64),
                    prompt_tokens,
                    uncached_tokens: inp,
                    cached_tokens: cache,
                    output_tokens,
                    created_at: s.created_at.clone(),
                    compacted: saw_explicit_checkpoint_since_last_planner || implicit_drop,
                });
                saw_explicit_checkpoint_since_last_planner = false;
            }
        }
    }

    // Spawned subagents
    let mut spawned_subagents: Vec<SpawnedSubagent> = Vec::new();
    let mut seen_sub_ids: HashSet<String> = HashSet::new();
    for (i, s) in steps.iter().enumerate() {
        if s.step_type.as_deref() != Some("PLANNER_RESPONSE") {
            continue;
        }
        let Some(tool_calls) = s.tool_calls.as_ref() else {
            continue;
        };
        for (t_idx, tc) in tool_calls.iter().enumerate() {
            if tc.name.as_deref() == Some("invoke_subagent") {
                let result_step = steps.get(i + 1 + t_idx);
                let result_text = result_step
                    .and_then(|st| st.content.as_deref())
                    .unwrap_or("");
                let sub_id = find_first_uuid(result_text);
                let should_push = match &sub_id {
                    Some(id) => seen_sub_ids.insert(id.clone()),
                    None => true,
                };
                if should_push {
                    let args = tc.args.as_ref().and_then(|v| v.as_object());
                    let arg_str = |k: &str, default: &str| -> String {
                        args.and_then(|m| m.get(k))
                            .and_then(json_unquote)
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| default.to_string())
                    };
                    spawned_subagents.push(SpawnedSubagent {
                        type_name: arg_str("TypeName", "subagent"),
                        role: arg_str("Role", ""),
                        model: arg_str("Model", "inherit"),
                        conversation_id: sub_id,
                        step_index: s.step_index.unwrap_or(i as u64),
                    });
                }
            }
        }
    }

    // Baseline customization & system prompt estimation
    let first_prompt = planner_indices
        .first()
        .map(|&idx| {
            steps[idx].input_tokens.unwrap_or(0) + steps[idx].cache_read_tokens.unwrap_or(0)
        })
        .unwrap_or(0);

    let mut pre_first_user_est: u64 = 0;
    let mut pre_first_sys_est: u64 = 0;
    let pre_limit = planner_indices.first().copied().unwrap_or(steps.len());
    for st in steps.iter().take(pre_limit) {
        let tok = est_step_tokens(st);
        if st.step_type.as_deref() == Some("USER_INPUT") {
            pre_first_user_est += tok;
        } else {
            pre_first_sys_est += tok;
        }
    }

    let pre_steps_raw = pre_first_user_est + pre_first_sys_est;
    let pre_steps_clamped = if first_prompt > 0 {
        pre_steps_raw.min((first_prompt as f64 * 0.85).floor() as u64)
    } else {
        pre_steps_raw
    };
    let pre_scale = if pre_steps_raw > 0 {
        pre_steps_clamped as f64 / pre_steps_raw as f64
    } else {
        1.0
    };
    let first_user_tokens = (pre_first_user_est as f64 * pre_scale).round() as u64;
    let first_sys_msg_tokens = pre_steps_clamped.saturating_sub(first_user_tokens);
    let raw_fixed_baseline = first_prompt.saturating_sub(pre_steps_clamped);

    let (system_tools_base, system_prompt_base) = if raw_fixed_baseline > 0 {
        let st = ((raw_fixed_baseline as f64 * 0.42).round() as u64).min(9500);
        let sp = raw_fixed_baseline.saturating_sub(st);
        (st, sp)
    } else {
        (4800, 5200)
    };

    let mut buckets = ContextBuckets {
        system_tools: system_tools_base,
        system_prompt: system_prompt_base,
        customization_breakdown: None,
        ..Default::default()
    };

    let mut cumulative_input_tokens: u64 = 0;
    let mut cumulative_cached_tokens: u64 = 0;
    let mut cumulative_output_tokens: u64 = 0;
    for &idx in &planner_indices {
        cumulative_input_tokens +=
            steps[idx].input_tokens.unwrap_or(0) + steps[idx].cache_read_tokens.unwrap_or(0);
        cumulative_cached_tokens += steps[idx].cache_read_tokens.unwrap_or(0);
        cumulative_output_tokens += steps[idx].output_tokens.unwrap_or(0);
    }

    let fixed_baseline_sum = buckets.system_tools + buckets.system_prompt;
    for cp in &mut compactions {
        if cp.after_tokens.is_none() {
            let after = fixed_baseline_sum + cp.summary_tokens;
            cp.after_tokens = Some(after);
            cp.saved_tokens = cp.before_tokens.saturating_sub(after);
        }
    }

    let last_planner_idx = planner_indices.last().copied();
    let just_compacted_pending_planner = match (last_compaction_step_idx, last_planner_idx) {
        (Some(cp_idx), Some(lp_idx)) => cp_idx > lp_idx,
        (Some(_), None) => true,
        _ => false,
    };

    let mut heavy_tool_calls: Vec<HeavyToolCall> = Vec::new();
    let mut override_latest_cache: Option<(u64, u64, u64, f64, u64)> = None;

    if just_compacted_pending_planner {
        let cp_idx = last_compaction_step_idx.unwrap_or(0);
        buckets.checkpoint_buffer = last_checkpoint_summary_tokens;
        for st in steps.iter().skip(cp_idx + 1) {
            attribute_raw_step(
                st,
                est_step_tokens(st),
                None,
                &mut buckets,
                &mut heavy_tool_calls,
            );
        }
        let live_post_compact = buckets.active_total();
        let cp_step = &steps[cp_idx];
        turns_history.push(TurnPoint {
            step_index: cp_step.step_index.unwrap_or(cp_idx as u64),
            prompt_tokens: live_post_compact,
            uncached_tokens: live_post_compact,
            cached_tokens: 0,
            output_tokens: 0,
            created_at: cp_step.created_at.clone(),
            compacted: true,
        });
        override_latest_cache = Some((live_post_compact, live_post_compact, 0, 0.0, 0));
    } else if planner_indices.is_empty() {
        buckets.user_messages = pre_first_user_est;
        buckets.system_messages = pre_first_sys_est;
    } else {
        let anchor_pos = if let Some(cp_idx) = last_compaction_step_idx {
            planner_indices
                .iter()
                .position(|&p_idx| p_idx > cp_idx)
                .unwrap_or(0)
        } else {
            0
        };
        let anchor_step_idx = planner_indices[anchor_pos];
        let anchor_prompt = steps[anchor_step_idx].input_tokens.unwrap_or(0)
            + steps[anchor_step_idx].cache_read_tokens.unwrap_or(0);

        if last_compaction_step_idx.is_none() {
            buckets.user_messages = first_user_tokens;
            buckets.system_messages = first_sys_msg_tokens;
        } else {
            let max_fixed = (anchor_prompt as f64 * 0.85) as u64;
            if fixed_baseline_sum > max_fixed && fixed_baseline_sum > 0 {
                let s = (anchor_prompt as f64 * 0.85) / fixed_baseline_sum as f64;
                buckets.system_tools = (buckets.system_tools as f64 * s).round() as u64;
                buckets.system_prompt = (buckets.system_prompt as f64 * s).round() as u64;
            }
            let adjusted_fixed = buckets.system_tools + buckets.system_prompt;
            let available_post_fixed = anchor_prompt.saturating_sub(adjusted_fixed);

            let cp_idx = last_compaction_step_idx.unwrap_or(0);
            let mut post_cp_steps = Vec::new();
            let mut post_cp_est_sum: u64 = 0;
            for st in steps.iter().take(anchor_step_idx).skip(cp_idx + 1) {
                let est = est_step_tokens(st);
                post_cp_steps.push((st, est));
                post_cp_est_sum += est;
            }

            let cp_buf_weight = if last_checkpoint_summary_tokens > 0 {
                last_checkpoint_summary_tokens
            } else {
                available_post_fixed.saturating_sub(post_cp_est_sum)
            };
            let total_weight = cp_buf_weight + post_cp_est_sum;
            let mut allocated_post_cp: u64 = 0;

            if !post_cp_steps.is_empty() && available_post_fixed > 0 {
                for (st, est) in post_cp_steps {
                    let raw_tok = if total_weight <= available_post_fixed {
                        est
                    } else if total_weight > 0 {
                        ((available_post_fixed as f64 * est as f64) / total_weight as f64).round()
                            as u64
                    } else {
                        0
                    };
                    let tok = raw_tok.min(available_post_fixed.saturating_sub(allocated_post_cp));
                    allocated_post_cp += tok;
                    attribute_raw_step(st, tok, None, &mut buckets, &mut heavy_tool_calls);
                }
            }
            buckets.checkpoint_buffer = available_post_fixed.saturating_sub(allocated_post_cp);
        }

        // Attribute turn-by-turn deltas
        for k in (anchor_pos + 1)..planner_indices.len() {
            let prev_idx = planner_indices[k - 1];
            let cur_idx = planner_indices[k];
            let prev_step = &steps[prev_idx];
            let cur_step = &steps[cur_idx];
            let prev_prompt =
                prev_step.input_tokens.unwrap_or(0) + prev_step.cache_read_tokens.unwrap_or(0);
            let cur_prompt =
                cur_step.input_tokens.unwrap_or(0) + cur_step.cache_read_tokens.unwrap_or(0);
            let delta: i64 = cur_prompt as i64 - prev_prompt as i64;
            let prev_out = prev_step.output_tokens.unwrap_or(0);

            let mut intervening = Vec::new();
            let mut est_intervening_sum: u64 = 0;
            for st in steps.iter().take(cur_idx).skip(prev_idx + 1) {
                let est = est_step_tokens(st);
                intervening.push((st, est));
                est_intervening_sum += est;
            }

            let (actual_prev_out, actual_intervening) = if delta >= prev_out as i64 {
                (prev_out, (delta - prev_out as i64) as u64)
            } else if delta >= 0 {
                let d = delta as u64;
                let inter = d.min(est_intervening_sum);
                let out = d.saturating_sub(inter);
                (out, inter)
            } else {
                let mut rem_prune = (est_intervening_sum as i64 - delta).max(0) as u64;
                let take_think = buckets.agent_thinking.min(rem_prune);
                buckets.agent_thinking -= take_think;
                rem_prune -= take_think;
                if rem_prune > 0 {
                    let take_replies = buckets.agent_replies.min(rem_prune);
                    buckets.agent_replies -= take_replies;
                }
                (0, est_intervening_sum)
            };

            if actual_prev_out > 0 {
                let (replies, thinking, tool_args) =
                    split_planner_output(prev_step, actual_prev_out);
                buckets.agent_replies += replies;
                buckets.agent_thinking += thinking;
                buckets.tool_call_args += tool_args;
            }

            if actual_intervening > 0 && !intervening.is_empty() {
                let prev_calls = prev_step.tool_calls.as_deref().unwrap_or(&[]);
                let mut offset = 0usize;
                let mut allocated = 0u64;
                let count = intervening.len();
                let step_index = prev_step.step_index.unwrap_or(prev_idx as u64);

                for (m, (st, est)) in intervening.into_iter().enumerate() {
                    let tok = if m == count - 1 {
                        actual_intervening.saturating_sub(allocated)
                    } else if est_intervening_sum > 0 {
                        ((actual_intervening as f64 * est as f64) / est_intervening_sum as f64)
                            .round() as u64
                    } else {
                        ((actual_intervening as f64) / (count as f64)).round() as u64
                    };
                    allocated += tok;
                    attribute_raw_step(
                        st,
                        tok,
                        Some((prev_calls, &mut offset, step_index)),
                        &mut buckets,
                        &mut heavy_tool_calls,
                    );
                }
            }
        }

        // Final planner step + trailing steps
        let last_idx = *planner_indices.last().unwrap();
        let last_step = &steps[last_idx];
        let last_prompt =
            last_step.input_tokens.unwrap_or(0) + last_step.cache_read_tokens.unwrap_or(0);
        let last_out = last_step.output_tokens.unwrap_or(0);
        let (rep, thk, tca) = split_planner_output(last_step, last_out);
        buckets.agent_replies += rep;
        buckets.agent_thinking += thk;
        buckets.tool_call_args += tca;

        let mut trailing_tokens: u64 = 0;
        let last_calls = last_step.tool_calls.as_deref().unwrap_or(&[]);
        let mut trailing_offset = 0usize;
        let last_step_index = last_step.step_index.unwrap_or(last_idx as u64);
        for st in steps.iter().skip(last_idx + 1) {
            let tok = est_step_tokens(st);
            trailing_tokens += tok;
            attribute_raw_step(
                st,
                tok,
                Some((last_calls, &mut trailing_offset, last_step_index)),
                &mut buckets,
                &mut heavy_tool_calls,
            );
        }

        let exact_target = last_prompt + last_out + trailing_tokens;
        reconcile_active_buckets(&mut buckets, exact_target);
    }

    let used_tokens = buckets.active_total();
    let free_tokens = context_limit.saturating_sub(used_tokens);
    buckets.free_space = free_tokens;

    let used_percent = if context_limit > 0 {
        let pct = (used_tokens as f64 / context_limit as f64) * 100.0;
        round_1dp(pct.min(100.0))
    } else {
        0.0
    };

    let latest_planner = planner_indices.last().map(|&idx| &steps[idx]);
    let (
        last_prompt_total,
        last_input_uncached,
        last_cache_read,
        cache_hit_ratio_frac,
        last_output_tokens,
    ) = if let Some(ov) = override_latest_cache {
        ov
    } else {
        let uncached = latest_planner.and_then(|s| s.input_tokens).unwrap_or(0);
        let cached = latest_planner
            .and_then(|s| s.cache_read_tokens)
            .unwrap_or(0);
        let total = uncached + cached;
        let ratio = if total > 0 {
            cached as f64 / total as f64
        } else {
            0.0
        };
        let out = latest_planner.and_then(|s| s.output_tokens).unwrap_or(0);
        (total, uncached, cached, ratio, out)
    };

    let session_cache_hit_ratio = if cumulative_input_tokens > 0 {
        round_1dp((cumulative_cached_tokens as f64 / cumulative_input_tokens as f64) * 100.0)
    } else {
        0.0
    };

    heavy_tool_calls.sort_by_key(|a| std::cmp::Reverse(a.tokens));
    let total_tool_calls = heavy_tool_calls.len();
    heavy_tool_calls.truncate(15);

    let total_saved_tokens = compactions.iter().map(|c| c.saved_tokens).sum();
    let step_summaries = build_step_summaries(&steps, &compacted_step_positions);
    let last_step_at = steps.last().and_then(|s| s.created_at.clone());

    Ok((
        ConversationAnalysis {
            conversation_id: conv_id.to_string(),
            found: true,
            exact: !planner_indices.is_empty() && !just_compacted_pending_planner,
            model: model_info,
            context_limit,
            used_tokens,
            free_tokens,
            used_percent,
            step_count: steps.len(),
            planner_turn_count: planner_indices.len(),
            last_step_at,
            cache: CacheStats {
                last_prompt_tokens: last_prompt_total,
                since_last_cache_tokens: last_input_uncached,
                cached_tokens: last_cache_read,
                cache_hit_ratio: round_1dp(cache_hit_ratio_frac * 100.0),
                last_output_tokens,
                cumulative_input_tokens,
                cumulative_cached_tokens,
                session_cache_hit_ratio,
                cumulative_output_tokens,
            },
            compactions,
            total_saved_tokens,
            buckets,
            turns_history,
            heavy_tool_calls,
            total_tool_calls,
            step_summaries,
            spawned_subagents,
            db_busy: model_res.db_busy,
        },
        model_from_db,
    ))
}

fn read_file_utf8_lossy(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

fn load_combined_steps(
    full_path: &Path,
    compact_path: &Path,
    tfp: &TranscriptFingerprint,
) -> Vec<TranscriptStep> {
    if tfp.full.exists {
        let full_raw = read_file_utf8_lossy(full_path);
        let mut steps = parse_transcript_steps(&full_raw);
        if tfp.compact.exists && tfp.compact.mtime_ns > tfp.full.mtime_ns {
            let compact_raw = read_file_utf8_lossy(compact_path);
            let compact_steps = parse_transcript_steps(&compact_raw);
            if compact_steps.len() > steps.len() {
                steps.extend_from_slice(&compact_steps[steps.len()..]);
            }
        }
        steps
    } else if tfp.compact.exists {
        let compact_raw = read_file_utf8_lossy(compact_path);
        parse_transcript_steps(&compact_raw)
    } else {
        Vec::new()
    }
}

fn attribute_raw_step(
    st: &TranscriptStep,
    tok: u64,
    heavy_ctx: Option<(&[ToolCallJson], &mut usize, u64)>,
    buckets: &mut ContextBuckets,
    heavy_tool_calls: &mut Vec<HeavyToolCall>,
) {
    let stype = st.step_type.as_deref().unwrap_or("");
    let src = st.source.as_deref().unwrap_or("");
    if stype == "USER_INPUT" {
        buckets.user_messages += tok;
    } else if src == "SYSTEM" || stype == "SYSTEM_MESSAGE" || stype == "CHECKPOINT" {
        buckets.system_messages += tok;
    } else if stype == "GENERIC" {
        buckets.tool_call_outputs += tok;
        if let Some((calls, offset, step_index)) = heavy_ctx
            && tok > 0
        {
            let tc_opt = calls.get(*offset);
            *offset += 1;
            if let Some(tc) = tc_opt {
                let (label, target_path) = describe_tool_call_short(tc);
                heavy_tool_calls.push(HeavyToolCall {
                    step_index,
                    tool_name: tc.name.clone().unwrap_or_else(|| "tool".to_string()),
                    label,
                    target_path,
                    tokens: tok,
                });
            }
        }
    } else {
        buckets.system_messages += tok;
    }
}

fn build_step_summaries(
    steps: &[TranscriptStep],
    compacted_positions: &HashSet<usize>,
) -> Vec<StepSummary> {
    steps
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let step_type = s.step_type.clone().unwrap_or_else(|| "UNKNOWN".to_string());
            let source = s.source.clone().unwrap_or_default();
            let status = s.status.clone().unwrap_or_default();
            let uncached_tokens = s.input_tokens.unwrap_or(0);
            let cached_tokens = s.cache_read_tokens.unwrap_or(0);
            let prompt_tokens = uncached_tokens + cached_tokens;
            let output_tokens = s.output_tokens.unwrap_or(0);
            let est_tokens = est_step_tokens(s);
            let compacted = compacted_positions.contains(&idx);

            let summary = if let Some(tcs) = s.tool_calls.as_ref()
                && !tcs.is_empty()
            {
                tcs.iter()
                    .map(|tc| describe_tool_call_short(tc).0)
                    .collect::<Vec<_>>()
                    .join(", ")
            } else if let Some(content) = s.content.as_deref() {
                truncate_single_line(content, 80)
            } else {
                String::new()
            };

            StepSummary {
                step_index: s.step_index.unwrap_or(idx as u64),
                step_type,
                source,
                status,
                created_at: s.created_at.clone(),
                prompt_tokens,
                uncached_tokens,
                cached_tokens,
                output_tokens,
                est_tokens,
                compacted,
                summary,
            }
        })
        .collect()
}

fn describe_tool_call_short(tc: &ToolCallJson) -> (String, Option<String>) {
    let Some(name) = tc.name.as_deref().filter(|n| !n.is_empty()) else {
        return ("tool".to_string(), None);
    };
    let args = tc.args.as_ref().and_then(|v| v.as_object());
    let get_arg = |k: &str| -> String {
        args.and_then(|m| m.get(k))
            .and_then(json_unquote)
            .unwrap_or_default()
    };

    for key in ["Path", "AbsolutePath", "TargetFile"] {
        let p = get_arg(key);
        if !p.is_empty() {
            let base = PathBuf::from(&p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone());
            return (format!("{name} ({base})"), Some(p));
        }
    }

    for key in ["toolSummary", "toolAction"] {
        let s = get_arg(key);
        if !s.is_empty() {
            return (format!("{name}: {s}"), None);
        }
    }

    if name == "run_command" {
        let cmd = get_arg("CommandLine");
        if !cmd.is_empty() {
            let short = truncate_single_line(&cmd, 48);
            return (format!("run_command: {short}"), None);
        }
    }

    if name == "invoke_subagent" {
        let tn = get_arg("TypeName");
        if !tn.is_empty() {
            let role = get_arg("Role");
            let label = if role.is_empty() {
                format!("subagent: {tn}")
            } else {
                format!("subagent: {tn} ({role})")
            };
            return (label, None);
        }
    }

    if name == "call_mcp_tool" {
        let srv = get_arg("ServerName");
        let tool = get_arg("ToolName");
        if !srv.is_empty() && !tool.is_empty() {
            return (format!("mcp: {srv}/{tool}"), None);
        }
    }

    (name.to_string(), None)
}

fn json_unquote(v: &serde_json::Value) -> Option<String> {
    let raw = match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Null => return None,
        other => other.to_string(),
    };
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        serde_json::from_str::<String>(&raw)
            .ok()
            .or_else(|| Some(raw[1..raw.len() - 1].to_string()))
    } else {
        Some(raw)
    }
}

fn truncate_single_line(text: &str, max_chars: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        collapsed.chars().take(max_chars).collect()
    }
}

fn round_1dp(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn find_first_uuid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.len() < 36 {
        return None;
    }
    for i in 0..=bytes.len() - 36 {
        let slice = &bytes[i..i + 36];
        if slice[8] == b'-'
            && slice[13] == b'-'
            && slice[18] == b'-'
            && slice[23] == b'-'
            && slice
                .iter()
                .enumerate()
                .all(|(idx, &b)| matches!(idx, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
        {
            return Some(String::from_utf8_lossy(slice).to_string());
        }
    }
    None
}
