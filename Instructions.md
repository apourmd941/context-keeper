# context-keeper — setup and usage

A step-by-step guide to getting `context-keeper` running locally and wiring it into Claude Code so the agent can pull only the past-conversation context it needs.

This guide assumes macOS on Apple Silicon. Linux is functionally similar — replace `brew install` with your package manager and skip the launchd/keychain mentions.

> **Note.** `./start.sh` is the recommended launcher (§5) — it assigns ports from the shared registry when one is present and falls back to fixed local ports otherwise. The Rust crates use SQLite + a flat-file vector store by default; no database server is required.

---

## 1. Prerequisites

You need:

- macOS (tested on Darwin 25.x, Apple Silicon)
- [Homebrew](https://brew.sh)
- A working `~/.claude/projects/` directory — i.e., you've already used Claude Code at least once in some project. The tool indexes those existing transcripts.
- Optional: an `ANTHROPIC_API_KEY` for LLM-assisted features (per-session summaries, topic naming) **when running outside the Selran orchestrator**. With the orchestrator running, those features route through it and no key is needed — see §3c and §9. Local embeddings via the bundled BGE model never need a key; everything else works fully offline.

Install the toolchain in one shot:

```bash
brew install just pnpm rustup protobuf
rustup default stable
```

> **Heads-up — Homebrew's `rustup` is keg-only.** It does NOT symlink `cargo` into `/opt/homebrew/bin`, so a fresh shell will say `bash: cargo: command not found` even though rustup installed successfully. Add it to your PATH:
>
> ```bash
> # bash:
> echo 'export PATH="/opt/homebrew/opt/rustup/bin:$PATH"' >> ~/.bash_profile
> source ~/.bash_profile
>
> # zsh (default macOS shell):
> echo 'export PATH="/opt/homebrew/opt/rustup/bin:$PATH"' >> ~/.zshrc
> source ~/.zshrc
> ```
>
> Or for just the current shell session: `export PATH="/opt/homebrew/opt/rustup/bin:$PATH"`.

Verify:

```bash
cargo --version       # 1.85+ expected
pnpm --version        # 9+ expected
protoc --version      # any 3.x — needed only for some optional deps
```

---

## 2. Get the code and build

```bash
cd ~/projects                         # or wherever you keep projects
# (the repo lives at ~/context-keeper if you followed along
#  with the project; otherwise: git clone <url> context-keeper)
cd context-keeper

pnpm install                            # frontend deps (~150MB node_modules)
cargo build --release --bin ck          # ~30-60s on a clean checkout
```

This produces `./target/release/ck` — the only binary you'll run from now on. The
`start.sh` launcher (§5) requires this build to exist before it will run.

---

## 3. First-time indexing

Indexing happens in three stages. Each step is incremental and idempotent — re-running is cheap because of content-addressed caches.

### 3a. Walk transcripts (fast metadata pass)

```bash
./target/release/ck doctor
```

This walks every `.jsonl` under `~/.claude/projects/`, parses defensively (unknown record types are tallied, never panicked on), and writes one `Session` JSON record per session under `~/.context-keeper/derived/sessions/`. SQLite metadata gets populated at `~/.context-keeper/index/meta.sqlite`.

You should see a table like:

```
PROJECT                            SESSION              MSGS  USER  ASST  TITLE
-Users-me-Development              ca9ba87c-...           481    140   229  context-keeper-v01-plan
-Users-me-Documents-same-table     ea09fea1-...            31     14    17  Review project setup
...
```

If `Unknown record types: ...` appears, that's the schema-drift detector noticing a Claude Code record type the parser hasn't seen — it's not an error.

### 3b. Embed every chunk (downloads BGE-small ~130MB on first run)

```bash
./target/release/ck doctor --with-embeddings
```

The first run downloads `bge-small-en-v1.5` (a 384-dimensional embedding model) to `~/.context-keeper/cache/models/`. After that, embeddings are cached per-text in `~/.context-keeper/cache/embeddings/<model>/<sha256>.bin`, so re-running is essentially free.

You'll see per-session lines like:

```
-Users-me-Development              ca9ba87c-...      481  140  229  context-keeper-v01-plan
    chunks: 241   embedded: 241 new, 0 cache hits
```

On the second run those same sessions will report `0 new, N cache hits`.

### 3c. Cluster into topics + cross-topic edges

```bash
./target/release/ck cluster
```

Hand-rolled DBSCAN over the chunk embeddings, per project. Produces a topic graph at `~/.context-keeper/derived/topics/` plus edge JSONs at `~/.context-keeper/derived/edges/`.

`ck cluster` can also ask an LLM to give each topic a short name (~3–6 words) — much more readable than the auto-labels. Names cache by topic id, so re-clustering after small changes doesn't pay the LLM cost again. There are two ways the LLM call is routed:

- **Via the Selran orchestrator (default in a managed deployment).** When the orchestrator is reachable on its loopback (`$SELRAN_ORCHESTRATOR_URL`, default `http://127.0.0.1:15454`), topic naming routes through `POST /v1/generate`. The orchestrator holds the key, picks the model, and enforces the egress ceiling — **no `ANTHROPIC_API_KEY` needed**.
- **Direct to Anthropic (dev/offline fallback).** Set `ANTHROPIC_API_KEY` and the call goes straight to `api.anthropic.com`. This path is kept only for development and offline use.

```bash
# Managed: orchestrator running — just cluster, no key required.
./target/release/ck cluster

# Dev/offline fallback: route directly to Anthropic.
export ANTHROPIC_API_KEY=sk-ant-...    # one-time, or add to your shell profile
./target/release/ck cluster
```

Without either path available, `ck cluster` falls back to centroid-text auto-labels (see §11).

---

## 4. Try a search from the CLI

```bash
./target/release/ck search "the chunker design decisions"
```

Returns the top 10 chunks ranked by cosine similarity, with a snippet, session title, and elapsed time at the bottom. Typical hot-cache search: ~3 ms embed + ~1 ms scan. Cold start (loading the embedder) adds ~250 ms.

```bash
# narrow to a project
./target/release/ck search "lance recursion limit" \
  --project=-Users-me-Development \
  --limit=5
```

---

## 5. Run the daemon (and the UI)

The daemon does three things:

- Watches `~/.claude/projects/` and re-indexes any changed `.jsonl` within ~250 ms (debounced).
- Serves an HTTP API (loopback only — never exposed).
- Broadcasts pipeline events on a WebSocket at `/v1/ws` for any subscriber.

### Recommended: `./start.sh`

The simplest way to bring up both services (backend daemon + mind-map UI) is the launcher script at the repo root:

```bash
cd ~/context-keeper
./start.sh
```

What it does:

- Requires `./target/release/ck` to already be built (§2). If it's missing, build it first with `cargo build --release --bin ck`.
- Asks a shared **port registry service at `127.0.0.1:11999`** for two ports: slot 0 = backend (`ck daemon`, the HTTP/WS API) and slot 1 = frontend (the Vite mind-map UI, which proxies `/v1` → daemon). **Ports are no longer hardcoded to 7421 / 5173.**
- Prints the actual assigned URLs on startup, e.g. `http://127.0.0.1:<backend>/v1/health` and `http://localhost:<frontend>`.
- Is idempotent — re-running restarts both services cleanly.
- Writes PIDs to `.app.pid` and logs to `.daemon.log` and `.web.log`, and traps Ctrl-C for a clean shutdown.

Stop both services with:

```bash
./stop.sh
```

Verify the backend is up using the URL `start.sh` printed:

```bash
curl http://127.0.0.1:<backend>/v1/health
# {"status":"ok","sessions":N,"chunks":M}
```

### Under the hood: running the daemon manually

If you'd rather start the daemon by hand (e.g. to watch its stdout directly), run the binary from the repo root:

```bash
cd ~/context-keeper           # ./target/release/ck is relative to
                                         # the repo root
./target/release/ck daemon &
```

(Or run it by absolute path from anywhere: `~/context-keeper/target/release/ck daemon &`.)

Run bare like this, the daemon binds the historical fixed port `127.0.0.1:7421`. Wait for `axum listening` in the log, then verify:

```bash
curl http://127.0.0.1:7421/v1/health
# {"status":"ok","sessions":N,"chunks":M}
```

### Live re-index demo

In another terminal, start a fresh Claude Code session somewhere — even just `cd /tmp && claude`. As you type each turn, the daemon's log will show:

```
INFO ck_pipeline: watcher batch n=1
INFO ck_pipeline: indexed session=<id> chunks=42 new=1 hits=41 is_sidechain=false
```

Each `new=N` is a freshly-embedded chunk; the rest are cache hits.

### Stopping the daemon

If you launched with `./start.sh`, just run `./stop.sh` (it reads `.app.pid`). If you started the daemon manually:

```bash
pkill -f "release/ck daemon"
```

(Plain `kill $(pgrep -f "ck daemon")` often grabs a wrapper shell and leaves the actual `ck` process orphaned holding its port. `pkill -f` is reliable.)

---

## 6. Wire it into Claude Code (MCP tool)

This is the main payoff. Once wired, any Claude Code session in any directory can call `recall(query)` as an MCP tool and get back the top-K chunks from your entire conversation history, MMR-diversified and packed under a token budget.

```bash
# the daemon must be running for tool calls to succeed
./start.sh                                  # or: ./target/release/ck daemon &

# register the MCP server (one-time)
claude mcp add --scope user context-keeper -- $(pwd)/target/release/ck mcp
```

The MCP shim forwards to the daemon's HTTP API. It defaults to `http://127.0.0.1:7421`; if `start.sh` assigned the backend a different port (slot 0), point the shim at it by setting `CK_DAEMON_URL` to the URL `start.sh` printed.

Now in any Claude Code session:

> "Use the `recall` tool to find what we decided about the chunker design."

The agent calls `recall("the chunker design")` over MCP. The shim forwards to the daemon, which embeds the query, runs MMR + token-budget packing, and returns ranked chunks with session id, title, score, and the chunk text. Three tools are advertised:

| Tool | Purpose |
| --- | --- |
| `recall(query, limit?, project?, token_budget?, mmr_lambda?)` | Top-K chunks with provenance. Default budget 4000 tokens, lambda 0.6. |
| `list_sessions(project?, limit?)` | Most-recent sessions, optionally filtered by project. |
| `list_projects()` | All indexed projects with counts and last-seen timestamps. |

If the daemon isn't running when a tool is called, the response is `{isError: true}` with a message telling you to start it.

---

## 7. Auto-recall hook (ambient context)

Section 6 puts you in control — you or the agent explicitly call `recall("...")` when context would help. The auto-recall hook flips that: for every prompt you submit in Claude Code, a small shell script queries the daemon for relevant chunks and injects them as additional context **before the model sees your prompt**. No tool call, no friction; relevant past context is just there.

### What it does

On every `UserPromptSubmit` event:

1. POSTs your prompt text to `http://127.0.0.1:7421/v1/recall` scoped to the current project (cwd → project_id).
2. Filters chunks by relevance score (default ≥ 0.60) and excludes any from the current session (so it can't echo itself).
3. Injects up to 5 chunks (~1500 token budget) prefixed with a `[ck-recall]` header that shows chunk count, returned tokens, total corpus size, and top score.

If the daemon is down, jq is missing, the prompt is < 4 words, or nothing clears the threshold, the hook exits silently. Your prompt always goes through.

### Enable

The hook script ships with the repo at `scripts/auto-recall-hook.sh`. Wire it into Claude Code by adding a `UserPromptSubmit` hook to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/ABSOLUTE/PATH/TO/context-keeper/scripts/auto-recall-hook.sh"
          }
        ]
      }
    ]
  }
}
```

Replace the path with the absolute path on your machine (the hook command must be an absolute path; `~` is not expanded). Restart Claude Code so settings reload at startup.

### Verify it's firing

Start a fresh Claude Code session in an indexed project and ask something specific to past work, e.g. *"what did we decide about the chunker?"*. The model's first answer should reference details from past sessions without your having to invoke `recall` manually.

To inspect what the hook injected, the model sees a `<system-reminder>` block whose first line reads something like:

```
[ck-recall] 4 chunk(s), ~298 tokens — searched 866 indexed chunks. Top score 0.78.
```

That header doubles as your savings indicator: roughly `corpus_tokens − returned_tokens` is what would have been loaded if you'd handed the agent the source transcripts directly.

### Tunable env vars

Set these in your shell profile (or in a wrapper script if you want per-project tuning):

| Var | Default | Purpose |
| --- | --- | --- |
| `CK_HOOK_SCORE_THRESHOLD` | `0.60` | Minimum cosine score for a chunk to be injected. Raise to 0.70+ for stricter filtering. |
| `CK_HOOK_LIMIT` | `5` | Max chunks returned. |
| `CK_HOOK_TOKEN_BUDGET` | `1500` | Token budget passed to the daemon (caps total injected text). |
| `CK_HOOK_MIN_WORDS` | `4` | Skip prompts shorter than this. Avoids burning tokens on "ok"/"yes". |
| `CK_HOOK_SCOPE` | `project` | `project` filters to the current cwd; `global` searches across all indexed projects. |
| `CK_DAEMON_URL` | `http://127.0.0.1:7421` | Override daemon address. |

### Disable

Remove the `UserPromptSubmit` entry from `~/.claude/settings.json` (or the whole `hooks` block). The script itself can stay on disk — it does nothing unless registered.

---

## 8. Open the mind map (web UI)

If you launched with `./start.sh` (§5), the UI is already running — open the
`http://localhost:<frontend>` URL it printed and skip the rest of this section.

To run the UI on its own (without `start.sh`):

```bash
cd ~/context-keeper           # MUST be the repo root — pnpm reads
                                         # pnpm-workspace.yaml from cwd, so
                                         # running this from ~/projects
                                         # fails with [ERR_PNPM_NO_PKG_MANIFEST]
./target/release/ck daemon &             # if not already running
pnpm --filter ck-web dev
```

Run bare like this, Vite reads `CK_UI_PORT` and `CK_DAEMON_URL` from the
environment (set automatically by `start.sh`); when they're unset it falls back
to UI port `5173` and daemon `http://127.0.0.1:7421` — so the manual method
still works on the historical fixed ports.

Open `http://localhost:5173` (or the assigned port). You'll see:

- A row of project chips at the top (most-recent project auto-selected).
- A React Flow canvas showing project → topic → session as three tiers with edges between topics (similarity = solid blue, shared-file = yellow dashed) and from topics to sessions (grey dashed).
- Click any session node — a side panel slides in with session metadata, the AI summary (when present), and the full transcript.

The canvas refetches every 10 s. (Live WebSocket-driven updates are listed under known gaps in `README.md`.)

Important: Vite binds IPv6 loopback only. Use `localhost:<port>` in your browser, not `127.0.0.1:<port>`. Browsers handle this transparently; only `curl` and similar scripts trip on it.

---

## 9. Generate per-session summaries (optional)

```bash
# Managed: orchestrator running — no key required.
./target/release/ck summarize

# Dev/offline fallback: route directly to Anthropic.
export ANTHROPIC_API_KEY=sk-ant-...
./target/release/ck summarize
```

Walks every session, sends its chunks to an LLM, and writes a structured summary (`text`, `bullets`, `decisions`, `artifacts`) back to the Session JSON. Cached on `input_hash` of the model + chunk texts — re-running is free unless the session has new turns.

Summarization routes the same way as topic naming (§3c): by default it goes through the Selran orchestrator (`POST /v1/generate` on `$SELRAN_ORCHESTRATOR_URL`, default `http://127.0.0.1:15454`), which holds the key and picks the model — **no `ANTHROPIC_API_KEY` needed**. Setting `ANTHROPIC_API_KEY` routes the call directly to `api.anthropic.com` instead; that direct path is kept only as a dev/offline fallback.

Cost: roughly $0.01–0.05 per session with Haiku, depending on length. Once cached, free.

Specific session:

```bash
./target/release/ck summarize --session ca9ba87c-0972-4f81-b0fe-bfbc470a5625
```

Force re-summarize even when cached:

```bash
./target/release/ck summarize --force
```

After running `ck summarize`, the side panel in the web UI shows the summary at the top of the transcript view.

---

## 10. Daily workflow

Once everything is set up, the typical loop is:

1. Start both services at the beginning of your work day:
   ```bash
   cd ~/context-keeper
   ./start.sh        # backend daemon + mind-map UI; prints the assigned URLs
   ```
2. Use Claude Code normally. The daemon indexes each turn within ~250 ms.
3. When you want to recall context in a future session, ask the agent to use the `recall` tool, or open the mind-map URL `start.sh` printed. (`start.sh` already serves the UI, so you don't need a separate `pnpm` invocation.)
4. Periodically (weekly is plenty) run `ck cluster` from the repo root to refresh topics — especially after long planning sessions that may have drifted into new themes.
5. At the end of the day (optional), `./stop.sh` shuts both services down cleanly.

The daemon is safe to leave running indefinitely. Everything writes atomically (temp + rename) so a power-off mid-write won't corrupt anything.

---

## 11. Troubleshooting

**"Address already in use" when starting the daemon manually.**
An orphaned `ck daemon` process is holding its port. Run `pkill -f "release/ck daemon"`, then check `lsof -i :7421` (or the slot-0 port `start.sh` assigned). If something else needs the port, pass `--bind 127.0.0.1:7422` instead. Launching with `./start.sh` avoids this — it cleans up prior instances and asks the port registry (`127.0.0.1:11999`) for a free port.

**The mind map shows "no topics yet — run `ck cluster`".**
You haven't run `ck cluster` yet, or you ran it before `ck doctor --with-embeddings` finished. The order is: `doctor --with-embeddings` → `cluster`.

**Topic labels look like `Tool call: TaskUpdate ...` instead of human names.**
No LLM naming path was available, so `ck cluster` fell back to centroid-text auto-labels. Either start the Selran orchestrator (default, no key needed) or set `ANTHROPIC_API_KEY` for the direct fallback, then re-run `ck cluster` — naming is cached per topic id, so you only pay the LLM cost once per cluster.

**`ck doctor --with-embeddings` is slow on first run.**
That's the BGE-small ONNX model downloading (~130MB). Re-runs are fast.

**`ck summarize` errors with "missing API key".**
The orchestrator wasn't reachable and no `ANTHROPIC_API_KEY` is set. Either start the Selran orchestrator (default path, no key) or set `ANTHROPIC_API_KEY` for the direct fallback, then re-run.

**Vite dev server won't start: "Port 5173 is already in use".**
Only applies to the bare `pnpm --filter ck-web dev` method (which defaults to 5173). Check `lsof -i :5173`, kill the orphan, then try again. Launching with `./start.sh` sidesteps this — it gets the frontend port (slot 1) from the port registry.

**The daemon log is empty and `curl /v1/health` fails.**
Daemon never started. Run it foreground: `./target/release/ck daemon` (no `&`). The first error message will be on stderr.

**MCP `recall` tool returns `{isError: true, content: [{text: "context-keeper daemon is not reachable..."}]}`.**
Either the daemon isn't running, or the shim is pointed at the wrong port. Start it with `./start.sh` (or `./target/release/ck daemon &`). If you launched via `start.sh` and the backend isn't on 7421, set `CK_DAEMON_URL` to the URL it printed (see §6).

**Schema drift warnings during `ck doctor`: `Unknown record types: agent-name: 4`.**
Not an error. The parser's defensive: it tallies unrecognized record types instead of crashing. If a particular type appears a lot, it's probably worth adding to `KNOWN_TYPES` in `crates/ck-transcript/src/lib.rs`, but functionality is unaffected.

---

## 12. Updating

When the repo updates:

```bash
cd ~/context-keeper
git pull
cargo build --release --bin ck

# restart both services (start.sh is idempotent — it restarts cleanly)
./start.sh
# (manual equivalent: pkill -f "release/ck daemon" && ./target/release/ck daemon &)

# Schema migrations are additive (CREATE TABLE IF NOT EXISTS), so existing
# data carries over. If a major version bumps schema-version, an explicit
# migration message will appear in the daemon log.
```

You don't need to re-run `ck doctor --with-embeddings` after every update — embeddings cache by content hash and survive code changes.

---

## 13. Resetting

If anything ever feels wrong, you can reset to a clean slate by deleting the local data root. Source transcripts under `~/.claude/projects/` are never touched.

```bash
./stop.sh                          # or: pkill -f "release/ck daemon"
rm -rf ~/.context-keeper           # wipes all derived data + caches
./target/release/ck doctor --with-embeddings
./target/release/ck cluster
```

A fresh full reindex of a 10-session corpus takes about 30 seconds.
