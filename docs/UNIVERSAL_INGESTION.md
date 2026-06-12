# Universal chat-history ingestion (Claude · Codex · ChatGPT · any tool)

**Goal.** context-keeper should keep the history of *any* AI coding/chat tool — not
just Claude Code. It discovers where each tool stores its conversation history,
parses + archives it, and runs the existing pipeline (chunk → embed → summarize →
graph → store). **The summarization model is the orchestrator's** (already true:
`ck-pipeline/promote.rs` wires `OrchestratorSummarizer` → Qwen via `/v1/generate`;
no app names a model or calls a provider directly). This doc specifies the
ingestion generalization.

## Architecture — a pluggable `TranscriptSource`

Everything downstream already consumes the normalized `ParsedSession`
(`ck-transcript`) → `RecordView { role, content_blocks }`. So generalization is a
**parser + discovery per tool**; the chunker/embedder/summarizer/graph need **no**
changes.

```
trait TranscriptSource {
    fn id(&self) -> &'static str;              // "claude_code" | "codex" | "chatgpt"
    fn history_roots(&self) -> Vec<PathBuf>;    // where this tool keeps history
    fn discover(&self) -> Result<Vec<DiscoveredSession>>;
    fn parse(&self, file: &Path) -> Result<ParsedSession>;
}
```

A `registry()` returns the sources whose roots exist on this machine. The daemon
iterates the registry (instead of only walking `~/.claude/projects`) and ingests
every source on the same schedule. `ParsedSession` gains a `source: &'static str`
tag (default `"claude_code"`) so provenance is preserved; `project_id` becomes
"tool + workspace" (Claude: the projects dir; Codex/ChatGPT: derived from `cwd` /
conversation id).

## Source 1 — Claude Code  *(implemented)*
- Root: `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` (+ `subagents/agent-*.jsonl`).
- Records: `{type:"user"|"assistant"|…, message:{role, content}, …}`.
- Already handled by `parse_session_file` / `parse_session_records`.

## Source 2 — Codex  *(format confirmed on this machine)*
- Roots: `~/.codex/sessions/**/rollout-*.jsonl` and `~/.codex/archived_sessions/rollout-*.jsonl`.
  Session index at `~/.codex/session_index.jsonl`.
- Records: `{timestamp, type, payload}` where `type` ∈:
  - `session_meta` (first record): `payload = {id, timestamp, cwd, originator, cli_version, source}` → `session_id` (= `id`), `started_at` (= `timestamp`), `cwd`.
  - `response_item` + `payload.type=="message"`: `{role: "user"|"assistant"|"developer", content: [ {type:"input_text"|"output_text"|"text", text} ]}` → one `RecordView{ role, content_blocks: [Text] }`. **Canonical conversation source.**
  - `event_msg` + `payload.type=="user_message"` `{message}` / `=="agent_message"` `{message}` → fallback text turns (use if no `response_item/message`).
  - `response_item` + `payload.type=="reasoning"` → `ContentBlock::Thinking`; `function_call` / `function_call_output` → `ContentBlock::ToolUse` (chunker already drops/handles these).
  - `event_msg` + `payload.type=="token_count"` `{info, rate_limits}` → `model_usage`.
  - `task_started` / `task_complete` / `turn_context` → ignored (tally under `stats.unknown_types`).
- `first_prompt` = first `user` message text; `ended_at` = last record timestamp.
- New module `ck-transcript/src/codex.rs`: `discover()` + `parse_codex_rollout(file) -> ParsedSession`.

## Source 3 — ChatGPT  *(export-based; format known, verify against a real export)*
- Root: user-provided **`conversations.json`** (Settings → Data controls → Export).
  Configurable import dir, e.g. `~/.selran/imports/chatgpt/`.
- Shape: top-level array; each conversation `{title, create_time, mapping:{<id>:{message:{author:{role}, content:{content_type, parts:[…]}, create_time}, parent, children}}}`.
- Walk the `mapping` tree from root following `children` in order → ordered turns →
  one `ParsedSession` per conversation (`session_id` = conversation id, `started_at`
  = `create_time`, `first_prompt` = first user part).
- New module `ck-transcript/src/chatgpt.rs`.

## Archival
Each source file is snapshotted into context-keeper's store keyed by
`sha256(source_file)` (the existing content-addressed model), so history is
retained even if the tool prunes/rotates its logs. Re-ingest is idempotent on the
hash (already how the Claude path dedups).

## Model = orchestrator (already correct — no change)
Summarization + topic-naming go through `OrchestratorSummarizer` (`/v1/generate`
→ Qwen, policy + ceiling enforced) as the **default** (`ck-pipeline/promote.rs`).
`AnthropicSummarizer` (direct `api.anthropic.com`) is **deliberately kept** as a
dev/offline fallback — analogous to datacore's `local_mode` escape hatch (which we
keep) — and is never wired in the managed deployment. **No change.** New sources
inherit this: they summarize through the orchestrator like the Claude path does.

## Implementation order (each step `cargo check`-clean)
1. `ck-transcript`: add `source: &'static str` to `ParsedSession` (default `"claude_code"`);
   add a `TranscriptSource` trait + `registry()`; refactor the Claude path to implement it.
2. `ck-transcript/src/codex.rs`: Codex discovery + `parse_codex_rollout` (mapping above).
3. `ck-daemon`: iterate `registry()` for discovery/ingest instead of only the Claude root.
4. `ck-transcript/src/chatgpt.rs`: ChatGPT export parser (verify against a real export).
5. Tests: one fixture per source → assert role/turn counts + first_prompt.

(The model side is already correct — see above — so there is no summarizer step.)

## Out of scope (works already)
- The MCP server (`ck-mcp`) is protocol-generic — any MCP client already uses it.
- The summarization model — orchestrator-managed (Qwen), provider-agnostic.
