# context-keeper

Cross-session memory for [Claude Code](https://claude.com/claude-code) —
local-first, with a searchable browser and map of everything you've ever
worked on.

By [Selran](https://selran.ai) — written and maintained by
**Padmalochan Singh** (padmalochan.singh@selran.ai), with **Aidin Eslampour**
(aidin.eslampour@selran.ai). MIT licensed.

Claude Code forgets everything between sessions. context-keeper walks every
transcript on disk, builds a semantic chunk index plus a topic graph, and
exposes a `recall(query)` MCP tool so future sessions pull exactly the past
context they need — ranked, diversity-re-ranked, token-budgeted. The agent can
also `remember` durable facts (and update or forget them) — as relevance-recalled
notes, **always-on rules**, or **file-glob-scoped** tips that only surface when
you touch a matching path; `recall` returns the right ones alongside the matching
past transcript. An optional hook injects relevant memory into every prompt
automatically, and a small web UI renders your projects, topics, and sessions as
an interactive map.

> Local-first: your transcripts are indexed on your machine by a local
> embedding model; the daemon binds `127.0.0.1` only; no telemetry. Secrets
> (API keys, tokens, private keys) are redacted from every chunk at ingest,
> before anything is embedded or stored. The only opt-in exception is
> LLM topic-naming/summaries — see [PRIVACY.md](PRIVACY.md).

## What it looks like

The primary view is a fast, searchable browser of every session, grouped by
project — filter by title instantly, or hit **Search content** (⌘K) to run
the same semantic recall Claude uses. A force-directed graph is the optional
secondary view. Dark, light, and system themes throughout.

| Browser — your sessions (dark) | Browser (light) |
|---|---|
| ![Session browser, dark](docs/screenshots/browser-dark.png) | ![Session browser, light](docs/screenshots/browser-light.png) |

The secondary graph view:

![Knowledge graph](docs/screenshots/graph-dark.png)

*(Screenshots use a small demo workspace, not real data.)*

## Use it from Claude Code (plugin)

```bash
# 1. The daemon (one-time; Rust toolchain required)
cargo install --git https://github.com/apourmd941/context-keeper ck
ck daemon &        # or: ./start.sh from a clone; or: ck autostart install

# 2. The plugin (ships the MCP wiring)
/plugin install context-keeper
```

Then ask Claude things like *"recall what we decided about the auth
refactor"* — or enable the auto-recall hook (§ Quickstart, step 8) and
relevant past context arrives with every prompt, no tool call needed.

**Summaries and topic names — no API key.** Because Claude itself is
connected through the plugin, it can do the LLM work in conversation: ask
*"summarize my recent sessions"* and Claude walks
`list_unsummarized_sessions` → `get_session_transcript` →
`save_session_summary`, and names topic clusters via `name_topic`. The
results persist to the same caches the optional API path uses and appear in
the UI immediately. `ANTHROPIC_API_KEY` (or an optional local model gateway)
remains relevant only for *unattended* batch summarization (`ck summarize`).

**No database to install.** context-keeper stores everything in an
embedded SQLite file and a flat-file vector index under
`~/.context-keeper/` — there is no Postgres, no server, and no external
service to set up. The first run downloads a small local embedding model
(~130 MB); after that it works fully offline.

## What it does

```
~/.claude/projects/*.jsonl                   # source of truth (Claude Code's transcripts)
        │
        ▼
  [parse + chunk]   defensive JSONL → typed records → token-budgeted chunks
        │
        ▼
  [embed]           bge-small-en-v1.5 via fastembed (local ONNX, 384-d)
        │           ╲
        ▼            ▶ flat-file vector index (~/.context-keeper/index/vectors.bin)
  [cluster]         hand-rolled DBSCAN over chunk embeddings, per project
        │
        ▼
  [name + edge]     LLM topic names (optional)
        │           topic-similarity, shared-file edges
        ▼
  [serve]           axum HTTP/WS (loopback) + the web UI
        │           MCP stdio shim (recall, summarize, name topics, …)
        ▼
  Claude Code session calls `recall("…")` and gets ranked, MMR-diversified,
  token-budgeted chunks back.
```

## Quickstart (macOS)

```bash
# One-time toolchain
brew install just pnpm rustup protobuf
rustup default stable

# Homebrew installs rustup keg-only — `cargo` won't be on your PATH yet.
# Add it (one of these, depending on your shell):
echo 'export PATH="/opt/homebrew/opt/rustup/bin:$PATH"' >> ~/.zshrc       # zsh (default)
echo 'export PATH="/opt/homebrew/opt/rustup/bin:$PATH"' >> ~/.bash_profile # bash
# Then open a new terminal, or `source` the file.

pnpm install

# Build
cargo build --release --bin ck

# 1. Walk transcripts (fast — metadata only)
./target/release/ck doctor

# 2. Embed every chunk (downloads BGE-small ~130MB on first run)
./target/release/ck doctor --with-embeddings

# 3. Cluster into topics + edges. Topics get plain text labels by default;
#    ask Claude to name them via the plugin, or set ANTHROPIC_API_KEY.
./target/release/ck cluster

# 4. Search
./target/release/ck search "the chunker design decisions"

# 5. Run the daemon + web UI together (recommended). Picks a free port
#    (default 7421, or $CK_PORT); start.sh prints the URL. Stop: ./stop.sh.
./start.sh

#    — or run them manually on the historical fixed ports —
# ./target/release/ck daemon &                 # daemon → 127.0.0.1:7421 (HTTP/WS + live re-index)
# curl http://127.0.0.1:7421/v1/health
# pnpm --filter ck-web dev                      # UI → http://localhost:5173 (proxies /v1 to the daemon)

# 7. Wire to Claude Code (MCP tool)
claude mcp add --scope user context-keeper -- $(pwd)/target/release/ck mcp
# from inside any Claude Code session, ask the agent to use the
# `recall`, `remember`, `list_memories`, `list_sessions`, or
# `list_projects` tools.

# 8. (Optional) Enable the auto-recall hook so every prompt gets
#    relevant past chunks injected as ambient context — no tool call
#    required. Add to ~/.claude/settings.json:
#
#    {
#      "hooks": {
#        "UserPromptSubmit": [
#          { "hooks": [
#              { "type": "command",
#                "command": "/ABSOLUTE/PATH/TO/scripts/auto-recall-hook.sh" } ] }
#        ]
#      }
#    }
#
#    Tunable via env vars: CK_HOOK_SCORE_THRESHOLD (0.60),
#    CK_HOOK_LIMIT (5), CK_HOOK_TOKEN_BUDGET (1500),
#    CK_HOOK_MIN_WORDS (4), CK_HOOK_SCOPE (project|global).
#    Silently no-ops when the daemon is down.
```

## Subcommands

| Command | What it does |
| --- | --- |
| `ck doctor [--dry-run] [--with-embeddings]` | Walk transcripts, persist sessions; with `--with-embeddings` also chunk + embed + index. |
| `ck cluster` | Run DBSCAN over the vector store; persist topics + edges. Topics get text labels by default; Claude can rename them via the plugin (`name_topic`), or set `ANTHROPIC_API_KEY` for `ck`-driven naming. |
| `ck search "<q>" [--project <id>] [--limit N]` | Local cosine search; prints ranked snippets + elapsed time. |
| `ck summarize [--session <id>] [--force] [--model <m>]` | Unattended batch summary via `ANTHROPIC_API_KEY` (or an optional local model gateway). For interactive use, ask Claude to summarize through the plugin — no key needed. Cached on `input_hash`. |
| `ck daemon [--bind 127.0.0.1:7421]` | Long-running indexer + HTTP/WS API. Watches `~/.claude/projects/` and re-indexes on change. Serves immediately; `/v1/health` reports `indexing` with progress during the boot scan. |
| `ck mcp` | JSON-RPC stdio shim; forwards tool calls to a running daemon. |
| `ck autostart install\|remove\|status` | Start-on-login via launchd (macOS). The agent runs `./start.sh`. |

## REST endpoints (`http://127.0.0.1:7421`, loopback-only)

```
GET  /v1/health
GET  /v1/projects
GET  /v1/sessions?project=&limit=
GET  /v1/sessions/:id
GET  /v1/sessions/:id/transcript
GET  /v1/graph?project=&include_sessions=
POST /v1/recall   {query, limit, project, token_budget, mmr_lambda}
GET  /v1/ws       (WebSocket — broadcasts session_indexed/daemon_ready events)
```

`recall` over-fetches 5× the limit (floor 50), runs MMR re-rank with
`lambda=mmr_lambda` (default 0.6), then greedily packs chunks under
`token_budget` (default 4000 tokens) using the per-chunk `tiktoken`
counts captured at chunking time. The response also carries a `stats`
block (candidate/returned counts, score range, timing); when auto-promote
is enabled it additionally kicks a background promotion check for
project-scoped, non-hook recalls.

## On-disk layout

```
~/.context-keeper/
  derived/                     # source of truth for derived data
    sessions/<sid>.json
    chunks/<sid>/<cid>.json
    topics/<tid>.json
    edges/<eid>.json
  index/
    meta.sqlite                # rebuildable
    vectors.bin                # rebuildable, bincode-serialized
  cache/
    embeddings/<model>/<sha>.bin
    llm-summaries/<sha>.json
    topic-names/<tid>.txt
    models/                    # downloaded ONNX
  state/
    schema-version
  sources/
    claude-projects -> ~/.claude/projects   # symlink, never copies
```

## Workspace layout

```
crates/
  ck-core         types, IDs, hashing
  ck-transcript   defensive JSONL parser; emits typed RecordViews
  ck-store        on-disk layout + SQLite metadata
  ck-chunk        token-budgeted chunker (1/user prompt, 400-tok windows)
  ck-embed        Embedder trait + LocalEmbedder (fastembed)
  ck-vector       flat-file linear-scan vector store + MMR
  ck-summarize    Summarizer trait (Anthropic API or an optional local gateway)
  ck-graph        DBSCAN + edge scoring + LLM topic naming
  ck-pipeline     watcher + index orchestration; shared DaemonState
  ck-api          axum HTTP/WS routes
  ck-mcp          JSON-RPC stdio shim
  ck-daemon       run loop tying it all together
apps/
  ck              the binary (clap subcommands)
  web             Vite + React + TS + Tailwind (browser + graph views)
```

## Notable design decisions

- **Hand-rolled DBSCAN** instead of `linfa-clustering` — `linfa 0.7` requires `ndarray 0.15` while `fastembed` pulls `ndarray 0.16`. Hand-rolled is microseconds at our scale and avoids the multi-version trait-impl conflict.
- **Flat-file vector store** instead of LanceDB — `lance 0.19.2`'s lib hits a rustc recursion-limit error on stable 1.95 that can only be fixed by patching lance itself. The flat-file store is faster than LanceDB at v0.1 corpus sizes; the `VectorStore` API is shaped so an ANN backend can be swapped in later.
- **Hand-rolled MCP** instead of `rmcp` — the stdio JSON-RPC protocol is small enough (~200 lines for the shim) that we control both ends and skip a churning third-party SDK.

## LLM features are optional — and need no API key

context-keeper's core — indexing, search, recall, and the UI — never needs a
network connection or an API key; embeddings run locally. The two LLM-assisted
extras are per-session summaries and topic naming, and the simplest way to get
them is **through the plugin**: ask Claude to summarize or name topics and it
does the work in your conversation, saving results back via MCP — no key. For
*unattended* batch runs (`ck summarize`), set `ANTHROPIC_API_KEY` or point
`SELRAN_ORCHESTRATOR_URL` at an optional local model gateway. With none of
these, topics get plain text labels and no summaries are generated. See
[PRIVACY.md](PRIVACY.md).

## Known gaps (deferred to v0.2)

- Voyage cloud embeddings (the `Embedder` trait is in place; only `LocalEmbedder` ships).
- **Universal ingestion.** A pluggable `TranscriptSource` for other assistants' histories is designed; only the Claude Code source ships today.

## Heads-up

- **`./start.sh` / `./stop.sh` are the recommended way to run it.** `start.sh` picks a free port (default `7421`, or `$CK_PORT`; a Selran port registry is used automatically if present), prints the URL, and writes `.app.pid` + `.daemon.log`/`.web.log` (git-ignored).
- **Vite dev server binds IPv6 loopback (`::1`) only.** Browsers handle this transparently; from a script, curl `localhost:<port>` or `[::1]:<port>`, not `127.0.0.1:<port>`.
- **First daemon start downloads the BGE-small ONNX model (~130MB)** to `~/.context-keeper/cache/models/`. The download progress is logged via `tracing`.
- **Killing `ck daemon`**: `pkill -f "release/ck daemon"` is reliable. `kill $(pgrep -f "ck daemon")` often picks up the wrapper shell instead of the actual binary, leaving an orphan that holds port 7421.
