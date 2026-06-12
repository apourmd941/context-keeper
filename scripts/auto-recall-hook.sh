#!/usr/bin/env bash
# context-keeper auto-recall hook for Claude Code's UserPromptSubmit event.
#
# Reads the hook JSON payload from stdin, queries the local ck daemon for
# chunks relevant to the user's prompt, and emits them back as
# additionalContext when at least one result clears the score threshold.
#
# Fails silently (exit 0, no stdout) on any error — the user's prompt must
# never be blocked by this hook.
#
# Tuning lives in ~/.context-keeper/config.toml (edit in the UI: Settings).
# The daemon applies [hook] score_threshold / limit / token_budget / scope to
# every hook-sourced recall, so this script sends a minimal body. Env vars
# remain as per-invocation OVERRIDES only:
#   CK_DAEMON_URL           default http://127.0.0.1:7421
#   CK_HOOK_SCORE_THRESHOLD override; otherwise server config (default 0.60)
#   CK_HOOK_LIMIT           override; otherwise server config (default 5)
#   CK_HOOK_TOKEN_BUDGET    override; otherwise server config (default 1500)
#   CK_HOOK_MIN_WORDS       client-side pre-filter, default 4
#   CK_HOOK_SCOPE           override; otherwise server config (default project)

set -u

DAEMON_URL="${CK_DAEMON_URL:-http://127.0.0.1:7421}"
SCORE_THRESHOLD="${CK_HOOK_SCORE_THRESHOLD:-}"   # empty → server config
LIMIT="${CK_HOOK_LIMIT:-}"                       # empty → server config
TOKEN_BUDGET="${CK_HOOK_TOKEN_BUDGET:-}"         # empty → server config
MIN_PROMPT_WORDS="${CK_HOOK_MIN_WORDS:-4}"
SCOPE="${CK_HOOK_SCOPE:-project}"                # project id still sent; server may widen to global

command -v jq >/dev/null 2>&1 || exit 0
command -v curl >/dev/null 2>&1 || exit 0

input=$(cat)

prompt=$(printf '%s' "$input" | jq -r '.prompt // empty' 2>/dev/null) || exit 0
cwd=$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null) || exit 0
session_id=$(printf '%s' "$input" | jq -r '.session_id // empty' 2>/dev/null) || exit 0

[ -z "$prompt" ] && exit 0

word_count=$(printf '%s' "$prompt" | wc -w | tr -d ' ')
[ "$word_count" -lt "$MIN_PROMPT_WORDS" ] && exit 0

prompt_trimmed=$(printf '%s' "$prompt" | head -c 2000)

project_id=""
if [ "$SCOPE" = "project" ] && [ -n "$cwd" ]; then
  project_id=$(printf '%s' "$cwd" | tr '/' '-')
fi

req_body=$(jq -nc \
  --arg q "$prompt_trimmed" \
  --arg p "$project_id" \
  --arg sid "$session_id" \
  --arg l "$LIMIT" \
  --arg tb "$TOKEN_BUDGET" \
  --arg ms "$SCORE_THRESHOLD" \
  '{query: $q, source: "hook"}
   + (if $l  == "" then {} else {limit: ($l | tonumber)} end)
   + (if $tb == "" then {} else {token_budget: ($tb | tonumber)} end)
   + (if $ms == "" then {} else {min_score: ($ms | tonumber)} end)
   + (if $p  == "" then {} else {project: $p} end)
   + (if $sid == "" then {} else {caller_session_id: $sid} end)')

resp=$(curl -sS -m 2 -X POST "$DAEMON_URL/v1/recall" \
  -H 'content-type: application/json' \
  -d "$req_body" 2>/dev/null) || exit 0

[ -z "$resp" ] && exit 0

filtered=$(printf '%s' "$resp" | jq \
  --arg sid "$session_id" \
  '.items // []
     | map(select($sid == "" or .session_id != $sid))') || exit 0

count=$(printf '%s' "$filtered" | jq 'length')
[ "$count" -eq 0 ] && exit 0

total_chunks=$(printf '%s' "$resp" | jq -r '.total_chunks // 0')
returned_tokens=$(printf '%s' "$filtered" | jq -r '[.[].token_count] | add // 0')
top_score=$(printf '%s' "$filtered" | jq -r '.[0].score | tostring | .[0:4]')

# Compression stats (added in M6.x). Older daemons without the stats
# block return null; treat that as "unknown" and skip the percent.
corpus_tokens=$(printf '%s' "$resp" | jq -r '.stats.corpus_tokens // empty')
compression_pct=""
if [ -n "$corpus_tokens" ] && [ "$corpus_tokens" -gt 0 ]; then
  compression_pct=$(printf '%s' "$resp" | jq -r '
    (.stats.compression_ratio // 0) * 100 | tostring | .[0:5]
  ')
fi

chunks_md=$(printf '%s' "$filtered" | jq -r '
  .[] |
  "### " + (.session_title // .session_id // "unknown") +
  " (" + ((.started_at // "") | tostring | .[0:10]) +
  ", score " + ((.score // 0) | tostring | .[0:4]) + ")\n" +
  (.text // "") + "\n"
')

if [ -n "$compression_pct" ]; then
  header=$(printf '[ck-recall] %s chunk(s), ~%s tok / %s corpus tok (%s%% returned). %s indexed chunks searched. Top score %s.' \
    "$count" "$returned_tokens" "$corpus_tokens" "$compression_pct" "$total_chunks" "$top_score")
else
  header=$(printf '[ck-recall] %s chunk(s), ~%s tokens — searched %s indexed chunks. Top score %s.' \
    "$count" "$returned_tokens" "$total_chunks" "$top_score")
fi
context=$(printf '%s\n\n%s' "$header" "$chunks_md")

jq -nc --arg ctx "$context" '{
  hookSpecificOutput: {
    hookEventName: "UserPromptSubmit",
    additionalContext: $ctx
  }
}'
