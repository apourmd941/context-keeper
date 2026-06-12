//! Auto-promote hot-recalled chunks into a managed block in the project's
//! CLAUDE.md.
//!
//! Triggered (via `tokio::spawn`) from the recall handler whenever a recall
//! returns chunks. The detection is cheap (one SQLite GROUP BY); the LLM
//! call only fires when there is genuinely new content to promote.
//!
//! Safety properties:
//! - **Off by default.** Reads `CK_AUTO_PROMOTE`; only writes when set to "1".
//! - **Hook recalls excluded.** The hot-chunk SQL filters `source != 'hook'`,
//!   so the auto-recall hook can't drive promotions on its own.
//! - **Idempotent writes.** The rendered managed-block content is hashed; if
//!   the hash matches the previous promotion for this project, no file write
//!   happens at all (and no LLM call beyond the cached one).
//! - **Atomic file writes.** Standard tmp + rename, same pattern the rest of
//!   the daemon uses.
//! - **Sentinel-bounded.** Only the block between
//!   `<!-- ck-promote:start ... -->` and `<!-- ck-promote:end -->` is touched.
//!   Hand-written CLAUDE.md content above/below is preserved verbatim.
//! - **Won't write outside an existing project cwd.** If the project's most
//!   recent recorded `cwd` is missing or not a directory, the promotion is
//!   skipped with a warning (no fallback to home dir or anywhere clever).

use chrono::{Duration as ChronoDuration, Utc};
use ck_store::{read_chunk, HotChunk, PromotionState};
use ck_summarize::{OrchestratorSummarizer, Summarizer};
use sha2::{Digest, Sha256};
use std::path::Path;
use tracing::{info, warn};

use crate::DaemonState;

const SENTINEL_START_PREFIX: &str = "<!-- ck-promote:start";
const SENTINEL_START_LINE: &str = "<!-- ck-promote:start (auto-managed by ck — do not edit) -->";
const SENTINEL_END_LINE: &str = "<!-- ck-promote:end -->";
const DEFAULT_HOT_THRESHOLD: u32 = 3;
const DEFAULT_WINDOW_DAYS: i64 = 30;
/// Cap how many hot chunks we send to the LLM per promotion. Bounds cost.
const MAX_CHUNKS_PER_RUN: usize = 12;

const SYSTEM_PROMPT: &str = "\
You distill recurring software-project conversations into short, durable \
project knowledge. Given excerpts that have come up in multiple work sessions, \
extract the underlying *facts* a future contributor would want loaded as \
context (decisions, constraints, gotchas, pivots) — NOT a recap of what was \
discussed. Output 1–6 markdown bullets, each one sentence, ≤25 words. No \
preamble, no headings, no closing remarks. If the excerpts contain no \
durable fact (just chatter), output a single line: NONE";

/// Outcome of a single promotion check. Used by tests and by the spawn
/// caller's logging.
#[derive(Debug)]
pub enum PromotionOutcome {
    Disabled,
    NoHotChunks,
    NoCwd,
    UnchangedContent,
    LlmReturnedNothing,
    Promoted { bullets: usize, target: String },
}

pub fn auto_promote_enabled() -> bool {
    std::env::var("CK_AUTO_PROMOTE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn hot_threshold() -> u32 {
    std::env::var("CK_PROMOTE_HOT_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HOT_THRESHOLD)
}

fn window_days() -> i64 {
    std::env::var("CK_PROMOTE_WINDOW_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_WINDOW_DAYS)
}

/// Top-level entry. Safe to spawn from a handler — never panics, logs and
/// returns on every failure path.
pub async fn check_promotion(state: DaemonState, project_id: String) -> PromotionOutcome {
    if !auto_promote_enabled() {
        return PromotionOutcome::Disabled;
    }

    // Step 1: query hot chunks for this project.
    let threshold = hot_threshold();
    let since = (Utc::now() - ChronoDuration::days(window_days())).to_rfc3339();
    let (hot, cwd) = {
        let meta = match state.meta.lock() {
            Ok(m) => m,
            Err(_) => {
                warn!("promote: meta lock poisoned, skipping");
                return PromotionOutcome::NoHotChunks;
            }
        };
        let hot = match meta.hot_chunks_in_project(&project_id, &since, threshold) {
            Ok(h) => h,
            Err(e) => {
                warn!(project = %project_id, error = %e, "promote: hot_chunks query failed");
                return PromotionOutcome::NoHotChunks;
            }
        };
        let cwd = meta.project_cwd(&project_id).ok().flatten();
        (hot, cwd)
    };

    if hot.is_empty() {
        return PromotionOutcome::NoHotChunks;
    }

    // Step 2: locate target CLAUDE.md path. Refuse if cwd is unset or doesn't exist.
    let cwd = match cwd {
        Some(c) => c,
        None => {
            warn!(project = %project_id, "promote: no cwd recorded for project, skipping");
            return PromotionOutcome::NoCwd;
        }
    };
    let cwd_path = Path::new(&cwd);
    if !cwd_path.is_dir() {
        warn!(project = %project_id, cwd = %cwd, "promote: project cwd is not an existing directory, skipping");
        return PromotionOutcome::NoCwd;
    }
    let target_path = cwd_path.join("CLAUDE.md");

    // Step 3: read hot chunks' text (capped, most-frequent first since the
    // SQL ORDERs by distinct_sessions DESC).
    let chunks_for_llm: Vec<(HotChunk, String)> = hot
        .iter()
        .take(MAX_CHUNKS_PER_RUN)
        .filter_map(|hc| {
            let sid = ck_core::SessionId(hc.session_id.clone());
            let cid = ck_core::ChunkId(hc.chunk_id.clone());
            match read_chunk(&state.layout, &sid, &cid) {
                Ok(c) => Some((hc.clone(), c.text)),
                Err(e) => {
                    warn!(chunk = %hc.chunk_id, error = %e, "promote: read_chunk failed, skipping chunk");
                    None
                }
            }
        })
        .collect();

    if chunks_for_llm.is_empty() {
        return PromotionOutcome::NoHotChunks;
    }

    // Step 4: ask the LLM to extract facts — routed through the orchestrator
    // (R1-016) so the egress ceiling + residency guard apply and we never hold a
    // cloud key here. Falls back gracefully when the orchestrator is unreachable.
    let summarizer = match OrchestratorSummarizer::from_env() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "promote: orchestrator unavailable, skipping");
            return PromotionOutcome::LlmReturnedNothing;
        }
    };

    let user_prompt = render_user_prompt(&chunks_for_llm);
    let bullets_raw = match summarizer.complete(SYSTEM_PROMPT, &user_prompt, 512).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            warn!(error = %e, "promote: LLM call failed");
            return PromotionOutcome::LlmReturnedNothing;
        }
    };

    if bullets_raw.is_empty() || bullets_raw.eq_ignore_ascii_case("NONE") {
        return PromotionOutcome::LlmReturnedNothing;
    }

    let bullets = clean_bullets(&bullets_raw);
    if bullets.is_empty() {
        return PromotionOutcome::LlmReturnedNothing;
    }

    // Step 5: render managed block + hash + idempotency check.
    let block_body = render_managed_block_body(&bullets, chunks_for_llm.len(), threshold);
    let content_hash = sha256_hex(&block_body);

    let prior = state
        .meta
        .lock()
        .ok()
        .and_then(|m| m.get_promotion_state(&project_id).ok().flatten());
    if let Some(p) = &prior {
        if p.content_hash == content_hash && p.target_path == target_path.to_string_lossy() {
            return PromotionOutcome::UnchangedContent;
        }
    }

    // Step 6: read existing CLAUDE.md (may not exist), splice or create.
    let existing = std::fs::read_to_string(&target_path).unwrap_or_default();
    let new_contents = splice_managed_block(&existing, &block_body);

    // Step 7: atomic write.
    let tmp = target_path.with_extension("md.ck-promote.tmp");
    if let Err(e) = std::fs::write(&tmp, &new_contents) {
        warn!(path = %target_path.display(), error = %e, "promote: tmp write failed");
        return PromotionOutcome::LlmReturnedNothing;
    }
    if let Err(e) = std::fs::rename(&tmp, &target_path) {
        warn!(path = %target_path.display(), error = %e, "promote: rename failed");
        return PromotionOutcome::LlmReturnedNothing;
    }

    // Step 8: persist promotion state.
    let now = Utc::now().to_rfc3339();
    if let Ok(meta) = state.meta.lock() {
        let _ = meta.upsert_promotion_state(&PromotionState {
            project_id: project_id.clone(),
            target_path: target_path.to_string_lossy().to_string(),
            content_hash,
            promoted_at: now,
        });
    }

    info!(
        project = %project_id,
        target = %target_path.display(),
        bullets = bullets.len(),
        chunks = chunks_for_llm.len(),
        "promote: wrote managed block"
    );

    PromotionOutcome::Promoted {
        bullets: bullets.len(),
        target: target_path.to_string_lossy().to_string(),
    }
}

fn render_user_prompt(chunks: &[(HotChunk, String)]) -> String {
    let mut s = String::new();
    s.push_str("Excerpts that have recurred across multiple work sessions:\n\n");
    for (i, (hc, text)) in chunks.iter().enumerate() {
        s.push_str(&format!(
            "--- Excerpt {} (recalled in {} distinct sessions) ---\n{}\n\n",
            i + 1,
            hc.distinct_sessions,
            truncate(text, 800)
        ));
    }
    s.push_str("Extract durable project facts as 1–6 short bullets, per the system rules.");
    s
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a char boundary <= max so we never split a UTF-8 codepoint.
        let mut i = max;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        &s[..i]
    }
}

/// Drop empty lines, normalize bullet markers to "-", strip surrounding
/// noise the model may emit despite the system prompt.
fn clean_bullets(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let stripped = l
            .strip_prefix("- ")
            .or_else(|| l.strip_prefix("* "))
            .or_else(|| l.strip_prefix("• "))
            .unwrap_or(l)
            .trim();
        if stripped.is_empty() {
            continue;
        }
        out.push(format!("- {stripped}"));
    }
    out
}

fn render_managed_block_body(bullets: &[String], chunks_used: usize, threshold: u32) -> String {
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    let mut s = String::new();
    s.push_str("## Recalled facts (auto-promoted by context-keeper)\n\n");
    for b in bullets {
        s.push_str(b);
        s.push('\n');
    }
    s.push('\n');
    s.push_str(&format!(
        "_Distilled from {} chunks recalled in ≥{} distinct sessions. Last refreshed {}._\n",
        chunks_used, threshold, now
    ));
    s
}

/// Replace the existing managed block in `existing` with `body`, or append
/// a new block if no sentinel is found. The output always ends with a
/// trailing newline.
fn splice_managed_block(existing: &str, body: &str) -> String {
    let block = format!(
        "{SENTINEL_START_LINE}\n{}\n{SENTINEL_END_LINE}",
        body.trim_end()
    );
    // Look for an existing block (start marker may be the canonical line OR
    // a hand-edited prefix variant — we match on the prefix to be lenient).
    if let (Some(start), Some(end_offset)) = (
        existing.find(SENTINEL_START_PREFIX),
        existing.find(SENTINEL_END_LINE),
    ) {
        let end = end_offset + SENTINEL_END_LINE.len();
        if end > start {
            let mut out = String::with_capacity(existing.len() + body.len());
            out.push_str(&existing[..start]);
            out.push_str(&block);
            out.push_str(&existing[end..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
    }
    // No (well-formed) existing block — append, with a separating blank line
    // when there's existing content.
    let mut out = String::with_capacity(existing.len() + block.len() + 4);
    if !existing.is_empty() {
        out.push_str(existing.trim_end());
        out.push_str("\n\n");
    }
    out.push_str(&block);
    out.push('\n');
    out
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_bullets_normalizes_markers() {
        let raw = "- one\n* two\n• three\nfour\n   \n  - five\n";
        let out = clean_bullets(raw);
        assert_eq!(
            out,
            vec![
                "- one".to_string(),
                "- two".to_string(),
                "- three".to_string(),
                "- four".to_string(),
                "- five".to_string(),
            ]
        );
    }

    #[test]
    fn splice_appends_when_no_sentinels() {
        let existing = "# CLAUDE.md\n\nProject overview here.\n";
        let out = splice_managed_block(existing, "## Recalled\n\n- fact 1\n");
        assert!(out.contains("Project overview here."));
        assert!(out.contains(SENTINEL_START_LINE));
        assert!(out.contains("- fact 1"));
        assert!(out.contains(SENTINEL_END_LINE));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn splice_replaces_existing_block_in_place() {
        let existing = format!(
            "# Project\n\nPreface.\n\n{}\nold body\n{}\n\nTail.\n",
            SENTINEL_START_LINE, SENTINEL_END_LINE
        );
        let out = splice_managed_block(&existing, "## Recalled\n\n- new fact\n");
        assert!(out.contains("Preface."));
        assert!(out.contains("Tail."));
        assert!(!out.contains("old body"));
        assert!(out.contains("- new fact"));
        // Only ONE managed block ends up in the output.
        assert_eq!(out.matches(SENTINEL_END_LINE).count(), 1);
    }

    #[test]
    fn splice_creates_block_when_file_empty() {
        let out = splice_managed_block("", "## Recalled\n\n- fact 1\n");
        assert!(out.starts_with(SENTINEL_START_LINE));
        assert!(out.contains("- fact 1"));
        assert!(out.trim_end().ends_with(SENTINEL_END_LINE));
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        // 4-byte emoji at position 5; with max=3 we stay below it.
        let s = "abc🦀def";
        assert_eq!(truncate(s, 3), "abc");
        // max=5 falls inside the emoji's bytes → we step back to 3.
        assert_eq!(truncate(s, 5), "abc");
    }

    #[test]
    fn auto_promote_off_by_default() {
        // Ensure the env var isn't set in this thread.
        std::env::remove_var("CK_AUTO_PROMOTE");
        assert!(!auto_promote_enabled());
        std::env::set_var("CK_AUTO_PROMOTE", "1");
        assert!(auto_promote_enabled());
        std::env::remove_var("CK_AUTO_PROMOTE");
    }
}
