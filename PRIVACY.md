# Privacy

context-keeper indexes the most sensitive data on your machine — your Claude
Code conversation history. This document states exactly what it does with it.

## The short version

Everything runs and stays on your computer. The daemon binds `127.0.0.1`
only, embeddings are computed by a local model, and nothing is uploaded,
collected, or phoned home. The one optional exception — LLM-generated topic
names and session summaries — only happens when you explicitly enable it,
and is clearly marked below.

## What it reads

- `~/.claude/projects/**/*.jsonl` — Claude Code's own transcripts, via a
  symlink. context-keeper never copies them; the originals remain the only
  source of truth.

## Secret redaction

Transcripts often contain credentials — an API key you pasted, a token a
tool printed. context-keeper scrubs high-confidence secrets (API keys,
GitHub/Slack/Stripe/AWS/Google tokens, JWTs, PEM private-key blocks, and
labelled `secret`/`password`/`token` assignments) from every chunk **at
ingest time — before it is embedded, stored, returned by `recall`, or shown
in the UI**. The secret never enters any derived artifact, and searching for
one can't surface it. Each match is replaced with `[REDACTED:<kind>]`. The
same scrub runs on any fact written through the `remember` tool, before it is
embedded or stored — written memories are not a hole in this guarantee.
Redaction runs locally with zero network calls. (It's high-confidence by
design, so an exotic credential format could slip through — treat it as a
strong safety net, not a guarantee.)

## What it stores, and where

Everything lives under `~/.context-keeper/` and is yours to inspect or
delete at any time (deleting it is always safe — the index rebuilds from
your transcripts):

- `derived/` — parsed sessions, chunks, topics, edges (JSON)
- `index/` — SQLite metadata + the vector index (rebuildable)
- `cache/` — content-addressed embeddings, the local ONNX model, and (if
  enabled) cached LLM summaries and topic names
- `config.toml` — your settings

## Network behavior

- **The daemon**: binds `127.0.0.1` only. Unreachable from the network.
- **Embeddings**: computed locally (BGE-small via ONNX). The model file is
  downloaded once from Hugging Face on first run (~130 MB) — that download
  is the only network request the core ever makes, and it contains none of
  your data.
- **The MCP bridge and the auto-recall hook**: talk to the local daemon
  over loopback. Nothing leaves the machine.
- **No telemetry, no analytics, no update checks.**

## Local-caller access control

By default the daemon is **loopback-only and keyless** — exactly as it has
always been. On a single-user machine, loopback binding plus the DNS-rebinding
guard (which rejects cross-origin writes from a web page you happen to visit)
are the protection. No API key, no account, no configuration. This is the
zero-config default and nothing about it changed.

For shared or hardened machines there is an **opt-in local token**, off by
default:

- A cryptographically-random token is generated on first daemon start and
  written to `~/.context-keeper/state/local-token` with `0600` (owner
  read/write only) permissions. It is local-only — it is never uploaded, is
  not a cloud credential, and requires no account or API key.
- Enforcement is **off unless you turn it on**: set `require_token = true` in
  `config.toml` (or `CK_REQUIRE_TOKEN=1` as an operator override). With
  enforcement off, the daemon behaves exactly as before — no token needed.
- When on, every `/v1/*` request must present the token via
  `Authorization: Bearer <token>` or a same-origin `ck_token` cookie. The one
  exception is `GET /v1/health` (a liveness probe), which stays open. The
  bundled MCP bridge and auto-recall hook read the token file and send it
  automatically; the web UI receives an `HttpOnly; SameSite=Strict` cookie
  from the daemon and authenticates itself. Token comparison is
  constant-time.

**What the token does and does not protect.** It blocks *other-user* processes
on the machine (they can't read your `0600` token file), cross-origin /
DNS-rebinding attempts, and any network access (the bind is still loopback).
It does **not** stop a process running as *you* that can read the token file —
that is an inherent limit of any local-token scheme, and no local daemon can
defend against a process with your own filesystem rights. If that is part of
your threat model, the protection has to come from the OS (separate user
accounts, sandboxing), not from this token.

## The one opt-in exception: LLM naming and summaries

`ck summarize` and topic auto-naming send chunk text to a language model to
produce labels and summaries. This happens **only** when:

- a local Selran orchestrator is running (requests stay on loopback and the
  orchestrator applies its own egress policy), **or**
- you have explicitly set `ANTHROPIC_API_KEY`, in which case the selected
  text is sent to the Anthropic API.

There is also a third path with no key at all: when Claude is connected
through the MCP plugin, it can write summaries and topic names itself during
your conversation (`save_session_summary`, `name_topic`). In that case the
transcript text goes only to the Claude client you are already chatting
with — no separate credential, no extra service.

If none of these are configured, topics get local centroid-text labels and
no summaries are generated. The core indexing, search, recall, and UI never
require a network connection.

## Contact

Padmalochan Singh · padmalochan.singh@selran.ai · Selran
Aidin Eslampour · aidin.eslampour@selran.ai · Selran
