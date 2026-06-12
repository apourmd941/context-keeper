# Integrate with Claude Code

Wire `context-keeper` into Claude Code so the agent can call its tools (`recall`, `list_sessions`, `list_projects`) directly inside any Claude session.

Integration uses MCP (Model Context Protocol). The `ck mcp` subcommand is a stdio JSON-RPC shim Claude Code spawns per session; it forwards every tool call to the running daemon over HTTP.

---

## Prerequisite checklist

Before running the integration command:

- [ ] The binary exists at `./target/release/ck` (run `cargo build --release --bin ck` if not).
- [ ] The daemon is running. The recommended way is `./start.sh`, which launches the daemon + UI and prints the daemon's assigned health URL (`http://127.0.0.1:<port>/v1/health`) — the port comes from the shared port registry (`127.0.0.1:11999`), so it is no longer guaranteed to be 7421. Verify with the URL `start.sh` prints. (Stop everything with `./stop.sh`.) The manual `./target/release/ck daemon &` method still defaults to `127.0.0.1:7421`. **Required** — without the daemon, every tool call returns `{isError:true}`.
- [ ] Indexing is done. At minimum: `./target/release/ck doctor --with-embeddings`. Recommended: also `./target/release/ck cluster` so topic listings are useful.
- [ ] You have Claude Code installed (`claude --version`).

> **If you'd rather not cd into the repo every time:**
>
> *Quick (this session): in another terminal, run*
>
> ```bash
> ~/context-keeper/target/release/ck daemon
> ```
>
> *Permanent (recommended): symlink it onto PATH so plain `ck` works:*
>
> ```bash
> ln -s ~/context-keeper/target/release/ck /opt/homebrew/bin/ck
> ```
>
> After the symlink, you can run `ck daemon`, `ck mcp`, `ck cluster`, etc. from any directory — and you can also pass plain `ck mcp` (instead of the full path) to `claude mcp add` in Step 2 below.

---

## Step 1 — Note the absolute path to the binary

`claude mcp add` needs an absolute path to the executable.

```bash
cd ~/context-keeper   # or wherever you cloned
echo "$(pwd)/target/release/ck"
```

Copy that path. Example output:

```
/Users/you/context-keeper/target/release/ck
```

---

## Step 2 — Register the MCP server

```bash
claude mcp add --scope user context-keeper -- ~/context-keeper/target/release/ck mcp
```

Substitute the path from Step 1. The breakdown:

- `claude mcp add` — the Claude Code subcommand for adding an MCP server.
- `--scope user` — register at user scope so the server is available in every Claude Code session, no matter what directory you start `claude` from. **Required if you want this to work everywhere.** See "scopes" below.
- `context-keeper` — the name the server appears under in Claude. Use any short identifier.
- `--` — separates `claude mcp add` flags from the command Claude will run.
- `/path/to/ck mcp` — the binary + the `mcp` subcommand. Claude Code spawns this per session over stdio.

### Scopes

`claude mcp add` defaults to **local** scope — i.e., the server is registered only for the directory you happen to be in when you run the command. That's almost certainly not what you want for `context-keeper`, since the whole point is to recall context across all your projects from any session.

| Flag | Where it lives | Available from |
| --- | --- | --- |
| `--scope user` (recommended for context-keeper) | `~/.claude.json` user section | Every Claude session, every directory |
| `--scope local` (default) | `~/.claude.json` per-project section | Only when running `claude` from the directory where you registered |
| `--scope project` | `.mcp.json` in the project | Anyone on the team who works in that project |

If you forgot `--scope user` and the registration landed at local scope, fix it with:

```bash
claude mcp remove context-keeper
claude mcp add --scope user context-keeper -- /path/to/ck mcp
```

---

## Step 3 — Verify the registration

```bash
claude mcp list
```

You should see `context-keeper` in the output. To inspect the exact configuration:

```bash
claude mcp get context-keeper
```

---

## Step 4 — Test from inside a Claude Code session

Start a Claude Code session in any directory:

```bash
cd /tmp && claude
```

Inside the session, ask the agent:

> "Use the `recall` tool to find anything I've discussed about token budgets in MCP."

Expected behavior:

1. Claude calls the `recall` tool with your query.
2. The shim forwards to the daemon's `/v1/recall` endpoint over HTTP. (The shim's daemon URL is configurable; with `start.sh` the port comes from the registry rather than being fixed at 7421.)
3. The daemon embeds the query, runs MMR + token-budget packing, returns ranked chunks.
4. Claude shows you the chunks (session id, score, snippet) and continues your conversation with that context.

If the daemon is not running, you'll see an error like:

```
context-keeper daemon is not reachable at 127.0.0.1:7421.
Run `ck daemon` in another terminal first.
```

That's expected — start the daemon and ask again. (The host:port shown reflects the shim's configured daemon URL. If you launched via `start.sh`, the daemon is on the registry-assigned port that `start.sh` printed, not necessarily 7421.)

---

## Step 5 — Confirm it survived a restart

Quit the Claude Code session, start a new one, and ask the agent to call any of the tools again. The MCP server registration is persistent.

---

## Updating the integration after a rebuild

The MCP registration stores the absolute path to the binary, not its contents. After `cargo build --release --bin ck`, the same path now points to the new binary — no re-registration needed. Just restart the daemon (`./stop.sh` then `./start.sh`) and the new code is live.

---

## Removing the integration

```bash
claude mcp remove context-keeper
```

Then optionally stop the daemon: `./stop.sh` (or, if you started it manually, `pkill -f "release/ck daemon"`).

---

## Troubleshooting

**Tool calls return "context-keeper daemon is not reachable".**
The daemon isn't running at the shim's configured daemon URL. Start it with `./start.sh` (which assigns a port from the registry and prints the health URL) or `./target/release/ck daemon &` (defaults to `127.0.0.1:7421`), then try again.

**`claude mcp list` doesn't show context-keeper after Step 2, or the agent says "There's no recall tool available to me".**
The registration probably landed at local scope (the default if you forgot `--scope user`). Confirm by running `claude mcp list` from the directory where you originally ran `claude mcp add` — if it appears there but not from other directories, that's a local-scope registration. Re-register at user scope:

```bash
claude mcp remove context-keeper
claude mcp add --scope user context-keeper -- /path/to/ck mcp
```

Then exit your current Claude session and start a new one — MCP tools are advertised at session start, so an existing session won't see a freshly-added server.

**Tool results show valid JSON but Claude doesn't seem to use them.**
The `recall` response includes both `content` (text the agent reads) and `structuredContent` (machine-readable). Make sure Claude is set up to surface tool results — for newer Claude Code versions this is automatic. You can also explicitly ask: "Show me the raw output of the last `recall` tool call."

**The agent doesn't know `recall` exists.**
Open a fresh Claude Code session — MCP tools are advertised at session-start during the `tools/list` exchange. Once advertised, the agent is aware of them and can choose to call them.

**Tool calls work but return zero hits.**
You haven't indexed. Run `./target/release/ck doctor --with-embeddings` to populate the vector store, then try again.

**Permission/path issues on `claude mcp add`.**
Use an absolute path. `~` expansion and relative paths can be flaky depending on how Claude Code spawns the subprocess.

---

## Tools the integration exposes

| Tool | Args | What it does |
| --- | --- | --- |
| `recall` | `query` (required), `limit` (default 10), `project`, `token_budget` (default 4000), `mmr_lambda` (default 0.6) | Top-K chunks with provenance, MMR-diversified, packed under the token budget. |
| `list_sessions` | `project` (optional), `limit` (default 20) | Recent sessions, most-recent first. |
| `list_projects` | (none) | All indexed projects with counts and last-seen timestamps. |

`recall` is the workhorse. The other two are mostly for the agent to discover what's available before deciding what to recall.
