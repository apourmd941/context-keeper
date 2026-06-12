# Start services

Quick reference for starting and stopping the daemon and web UI.

For first-time setup (toolchain, build, initial indexing), see `Instructions.md` first. This file assumes everything is already built at `./target/release/ck` and indexed at `~/.context-keeper/`.

---

## Quickest path — `./start.sh`

If you just want both services up, run the launcher from the repo root:

```bash
cd ~/context-keeper
./start.sh
```

It assigns ports from the shared Selran **port registry** (`127.0.0.1:11999`) — slot 0 = the `ck daemon` backend, slot 1 = the Vite mind-map UI — reclaims any stale instances on those ports, starts both, and **prints the assigned URLs**, e.g.:

```
• daemon API/WS : http://127.0.0.1:<backend>/v1/health
• mind-map UI   : http://localhost:<frontend>
```

Logs go to `.daemon.log` and `.web.log`; PIDs to `.app.pid`. Stop with `./stop.sh` (or Ctrl-C if you ran it in the foreground). Re-running `./start.sh` is a safe restart.

The rest of this file is the **manual** path — useful for understanding the pieces, debugging, or running a single service. The manual daemon/UI default to the historical fixed ports (`7421` / `5173`) when started bare.

---

## Step 1 — Verify the binary exists

```bash
ls -lh ./target/release/ck
```

If it's missing, build it:

```bash
cargo build --release --bin ck
```

> If `cargo` itself is missing (`bash: cargo: command not found`), rustup is installed but not on your PATH — Homebrew installs it keg-only. Quick fix for the current shell:
>
> ```bash
> export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
> ```
>
> Persistent fix: append that line to `~/.bash_profile` (bash) or `~/.zshrc` (zsh). Full toolchain notes are in `Instructions.md` Section 1.

---

## Step 2 — Start the daemon

The daemon is the only background service required for everything else (search, MCP, web UI) to work. Start it from the repo root:

```bash
cd ~/context-keeper           # ./target/release/ck is relative
./target/release/ck daemon &
```

The first start downloads the BGE-small ONNX model (~130 MB) to `~/.context-keeper/cache/models/`. Subsequent starts are instant.

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
> After the symlink, you can run `ck daemon`, `ck search "…"`, `ck cluster`, etc. from any directory.

---

## Step 3 — Verify the daemon is healthy

```bash
curl http://127.0.0.1:7421/v1/health
```

Expected output:

```json
{"status":"ok","sessions":N,"chunks":M}
```

If this returns nothing or "Connection refused", the daemon didn't start. Run it foreground to see the error:

```bash
./target/release/ck daemon
```

The first error message goes to stderr.

> **Auto-recall hook prerequisite.** If you've registered the `UserPromptSubmit` hook (see `Instructions.md` §7), it requires the daemon to be running. When the daemon is down the hook silently no-ops — your Claude Code prompts still go through, but no past-context chunks are injected. Healthy `/v1/health` here is also "hook is working." There's no separate process for the hook itself.

---

## Step 4 — Start the web UI (optional)

If you want the visual mind map:

```bash
cd ~/context-keeper           # MUST be the repo root, not
                                         # ~/projects — pnpm reads
                                         # pnpm-workspace.yaml from cwd
pnpm --filter ck-web dev
```

You should see:

```
VITE v5.4.21  ready in 108 ms
➜  Local:   http://localhost:5173/
```

Leave the terminal open — it's a foreground process. The Vite dev server proxies `/v1/*` requests to the daemon at `127.0.0.1:7421`, so the daemon (Step 2) must be running for the UI to show data.

---

## Step 5 — Verify the web UI

Open http://localhost:5173 in a browser. You should see:

- A row of project chips at the top (most-recent project auto-selected)
- A canvas with project, topic, and session nodes
- A `N nodes · M edges · Xms` line at the bottom-left

If the canvas says "no topics yet", run `./target/release/ck cluster` (in another terminal) to populate the topic graph.

---

## Stop services

If you started with `./start.sh`, the clean teardown is just:

```bash
./stop.sh
```

It reads the app's assigned port block from the registry (and the `.app.pid` file) and kills both services. The manual steps below are for daemon/UI you started by hand.

### Stop the daemon

```bash
pkill -f "release/ck daemon"
```

(Plain `kill $(pgrep -f "ck daemon")` often picks up a wrapper shell instead of the binary, leaving an orphaned `ck` process holding port 7421. `pkill -f` is reliable.)

Verify it's gone:

```bash
pgrep -f "release/ck daemon"
# (no output = daemon stopped)
```

### Stop the web UI

If you started it in the foreground, just `Ctrl-C` in its terminal.

If it's been backgrounded:

```bash
pkill -f "vite"
```

Verify the port is free:

```bash
lsof -i :5173
# (no output = port free)
```

---

## Restart after a code change

```bash
# Rebuild
cargo build --release --bin ck

# Stop the old daemon
pkill -f "release/ck daemon"
sleep 1

# Start the new one
./target/release/ck daemon &

# Verify
curl http://127.0.0.1:7421/v1/health
```

The web UI doesn't need restart — Vite hot-reloads on file save.

---

## Common service-startup errors

| Symptom | Cause | Fix |
| --- | --- | --- |
| `Error: bind 127.0.0.1:7421` | Old daemon still alive | `pkill -f "release/ck daemon"` then retry |
| `Port 5173 is already in use` | Old vite still alive | `pkill -f "vite"` then retry |
| `[ERR_PNPM_NO_PKG_MANIFEST] No package.json found in /Users/…/Development` | `pnpm` was run from outside the repo root (e.g. `~/Development`) | `cd ~/context-keeper` then re-run `pnpm --filter ck-web dev` |
| `vector store is empty` (from `ck doctor`) | Never ran `--with-embeddings` | `./target/release/ck doctor --with-embeddings` |
| Daemon log shows `watcher armed` but no events | No new session activity | Open Claude Code somewhere; events appear within 250 ms |
| `ck daemon` exits immediately with no obvious error | First-run model download failed (network issue) | Re-run; check network; the model lives at `~/.context-keeper/cache/models/` |
| Auto-recall hook injects nothing into Claude Code prompts | Daemon not running, or hook script not executable, or `jq` missing | Verify `curl http://127.0.0.1:7421/v1/health`; `chmod +x scripts/auto-recall-hook.sh`; `brew install jq` |

---

## Run all services in one shell (development)

```bash
# Terminal 1 — daemon (run from repo root)
cd ~/context-keeper
./target/release/ck daemon

# Terminal 2 — web UI (also from repo root; pnpm needs the workspace file)
cd ~/context-keeper
pnpm --filter ck-web dev

# Terminal 3 — your normal Claude Code work
cd /some/project && claude
```

Three windows, each foreground, easy to read logs and easy to Ctrl-C cleanly.
