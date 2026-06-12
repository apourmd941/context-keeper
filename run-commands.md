# Run commands

How to actually use `context-keeper` once it's set up. Two surfaces:

1. **Inside Claude Code** — talk to the agent and let it call the MCP tools.
2. **Web interface** — open the mind map in your browser.

If MCP isn't wired yet, see `integrate-with-claude.md` first. If the daemon isn't running, see `start-services.md` first.

---

## Part A — Use it from inside Claude Code

The agent sees three tools: `recall`, `list_sessions`, `list_projects`. You don't call them directly — you ask the agent in plain English and it picks the right tool. Below are example prompts that work well.

### A.1 — Recall context from past sessions

This is the main use case. Ask Claude to remember.

> "Use the `recall` tool to find what we decided about the chunker design."

> "Pull anything from past conversations about LanceDB or vector stores."

> "Recall how I configured the daemon's bind address. Pick at most 5 chunks."

> "Use recall with token_budget=1000 to keep it tight."

The agent calls `recall(query, ...)`, the daemon returns ranked chunks with session id + score + text, and the agent reads them as part of its working context for whatever follows.

### A.2 — List recent sessions

> "Show me the most recent five sessions in the context-keeper project."

> "List sessions from project `-Users-me-Documents-notes`."

The agent calls `list_sessions(project=..., limit=5)`. Useful when you don't remember a session's title and want to scan a few.

### A.3 — List all projects

> "What projects do I have indexed?"

> "Use `list_projects`."

Returns project ids, session counts, and last-seen timestamps. Useful for orientation when starting work in a new directory.

### A.4 — Combine with normal work

These tools shine when chained with the agent's regular work:

> "Recall what we decided about MMR lambda, then update the default in `crates/ck-api/src/lib.rs` to match."

> "List the recent sessions in this project, then summarize the last three in two sentences each."

> "Recall how we handled the daemon kill issue in the past, and apply the same pattern to the new vite kill code."

### A.5 — When the agent doesn't reach for recall on its own

If the agent doesn't think to use the tool, prompt it explicitly:

> "Before answering, check past conversations with `recall("...")`."

You can also drop a hint in the project's `CLAUDE.md` so future sessions know it's available:

```
This project has a `recall` MCP tool exposed by context-keeper.
Use it when the user asks "what did we decide about X?" or
"how did I solve Y last time?"
```

### A.6 — Tuning the recall behavior

The defaults (`limit=10`, `token_budget=4000`, `mmr_lambda=0.6`) are fine for most queries. Tweak as needed:

> "Recall with `mmr_lambda=0.2` so I get diverse results across sessions, not the same one repeated."

> "Recall with `token_budget=500` — I just want a quick reminder, not the full context."

> "Recall with `mmr_lambda=1.0` for pure relevance, no diversification."

---

## Part B — Use the web interface

The mind map gives you a visual, browse-y view of everything indexed.

### B.1 — Open it

Make sure both services are running. The simplest way is the launcher (see `start-services.md`):

```bash
./start.sh                 # starts the daemon + UI; prints the assigned URLs
```

(Or start them manually: `./target/release/ck daemon &` then `pnpm --filter ck-web dev`.)

Then open the **mind-map UI** URL that `start.sh` printed, e.g.:

```
http://localhost:<frontend-port>     # registry-assigned; 5173 when run bare via pnpm
```

Important: use `localhost`, not `127.0.0.1`. Vite binds IPv6 loopback only; browsers handle this transparently, scripts trip on it.

### B.2 — What you see

Top bar:

- **Project chips** — one per indexed project. The most-recently-active project is auto-selected on first load.
- **Show all** — toggles off the per-project filter and shows the global mesh of every project.
- **Search** — type to filter nodes by label/text.
- **View toggle** — switch between the tiered **mind map** (project → topics → sessions) and a **force-directed knowledge graph** of the same data.

Main canvas:

- **Project nodes** (grey, top tier) — one per project in scope.
- **Topic nodes** (blue, middle tier) — clusters of related chunks. Label is the LLM-generated name when `ANTHROPIC_API_KEY` was set during `ck cluster`, otherwise the centroid chunk's first sentence.
- **Session nodes** (orange, bottom tier) — individual Claude Code sessions in scope.
- **Edges**:
  - Solid blue, animated, weight-scaled = `topic-similarity` (cosine > 0.78 between topic centroids)
  - Yellow dashed = `shared-file` (≥3 chunks per topic mention the same file path or URL)
  - Grey solid = `contains-topic` (project owns this topic)
  - Grey dashed = `contains-session` (topic includes this session)

Bottom-left footer: `N nodes · M edges · Xms`.

### B.3 — Hover

Hover any node to see a tooltip:

- Project: full id + session count
- Topic: full description (the centroid chunk's text, ~240 chars)
- Session: project id + ended-at timestamp

### B.4 — Click into a session

Click any orange (session) node. A side panel slides in from the right:

- Header: session title + close button
- Metadata: id, project, message + chunk counts, started/ended timestamps
- Summary (when present): the full text + bullet points produced by `ck summarize`
- Transcript: every chunk in turn order, with role-tinted left borders (green=user, blue=assistant, orange=tool). Each chunk truncates at 800 chars (a known v0.2 polish item).

Close the panel with the `✕` in the top-right.

### B.5 — Pan, zoom, mini-map

The canvas has built-in React Flow controls in the bottom-left:

- `+` / `-` zoom buttons
- Lock toggle (prevents accidental drag of nodes)
- Fit-to-view button
- Mini-map in the bottom-right — pan-able and zoom-able, useful when the main canvas is zoomed in

Pan by drag-on-empty-canvas. Zoom by scroll. Standard React Flow gestures.

### B.6 — Refresh

The canvas auto-refetches the graph every 10 seconds. So if you open a Claude Code session in another terminal and it adds new chunks, the canvas picks them up within ~10s of the daemon indexing them.

(Live WebSocket-driven updates are listed in the v0.2 deferred list.)

---

## Quick recipe — "show me what I worked on yesterday"

```bash
# 1. Make sure services are running
./start.sh                              # prints the assigned UI URL

# 2. Open the mind map at the URL start.sh printed
open http://localhost:<frontend-port>   # 5173 if you ran pnpm bare instead

# 3. Find the project chip in the top bar; click it to scope.

# 4. Look at the bottom tier — sessions are sorted by ended_at descending,
#    so yesterday's work is at one end of the row.

# 5. Click each interesting session node to read it in the side panel.
```

---

## Quick recipe — "recall a specific past decision"

Inside Claude Code:

> "Use `recall("the decision to drop LanceDB and use a flat-file vector store", limit=3, mmr_lambda=1.0)` and summarize what we settled on."

The agent calls `recall`, gets the top-3 most-relevant chunks (pure relevance — no diversification), reads them, and writes you a short summary grounded in the actual past conversation.

---

## What the tool is good at

- "Did we decide X?" — recall surfaces the exact moment a decision was made.
- "Show me how I configured Y last time" — config recipes are usually one chunk.
- Onboarding a new project session by reviewing the mind map for adjacent context.
- Discovering forgotten work — the `shared-file` edges connect topics that touch the same files in different sessions.

## What it's not great at (yet)

- Pulling out long-form artifacts (it returns chunks, not full transcripts — for that, click into the session in the UI).
- Cross-language search (BGE-small-en is English-only; non-English content gets weak embeddings).
- Tool-call-heavy clusters often label as "Tool call: ..." in the auto-label mode — set `ANTHROPIC_API_KEY` and re-run `ck cluster` for human-readable names.
