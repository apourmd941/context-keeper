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

## The one opt-in exception: LLM naming and summaries

`ck summarize` and topic auto-naming send chunk text to a language model to
produce labels and summaries. This happens **only** when:

- a local Selran orchestrator is running (requests stay on loopback and the
  orchestrator applies its own egress policy), **or**
- you have explicitly set `ANTHROPIC_API_KEY`, in which case the selected
  text is sent to the Anthropic API.

If neither is configured, topics get local centroid-text labels and no
summaries are generated. The core indexing, search, recall, and UI never
require a network connection.

## Contact

Padmalochan Singh · padmalochan.singh@selran.ai · Selran
Aidin Eslampour · aidin.eslampour@selran.ai · Selran
