// Wire-format types from /v1/graph (backend tagged-enum + GraphEdge).

export type GraphNode =
  | {
      kind: 'project';
      id: string;
      label: string;
      sessions: number;
      cwd: string | null;
    }
  | {
      kind: 'topic';
      id: string;
      label: string;
      description: string;
      size: number;
      session_ids: string[];
      project_ids: string[];
    }
  | {
      kind: 'session';
      id: string;
      label: string;
      ai_title: string | null;
      is_sidechain: boolean;
      project_id: string;
      chunk_count: number;
      message_count: number;
      ended_at: string;
    };

export interface GraphEdge {
  from: string;
  to: string;
  kind: 'topic-similarity' | 'shared-file' | 'contains-topic' | 'contains-session';
  weight: number;
  evidence: string[];
}

export interface GraphResponse {
  nodes: GraphNode[];
  edges: GraphEdge[];
  elapsed_ms: number;
}

export interface ProjectSummary {
  id: string;
  sessions: number;
  last_seen: string | null;
}

export interface TranscriptEntry {
  chunk_id: string;
  turn_index: number;
  role: string;
  kind: string;
  text: string;
  token_count: number;
  started_at: string;
  tool_name: string | null;
}

export interface SessionRecord {
  id: string;
  project_id: string;
  is_sidechain: boolean;
  ai_title: string | null;
  first_prompt: string | null;
  message_count: number;
  started_at: string;
  ended_at: string;
  chunk_ids: string[];
  summary: {
    text: string;
    bullets: string[];
    decisions: string[];
    artifacts: string[];
  } | null;
}

const BASE = '/v1';

async function getJson<T>(path: string): Promise<T> {
  // C4: `credentials: 'same-origin'` so the HttpOnly `ck_token` cookie (set by
  // the daemon on the SPA HTML when local-token enforcement is on) rides along.
  // This is fetch's default for same-origin requests, but we set it explicitly.
  const r = await fetch(`${BASE}${path}`, { credentials: 'same-origin' });
  if (!r.ok) throw new Error(`${path}: ${r.status} ${r.statusText}`);
  return r.json() as Promise<T>;
}

export const api = {
  graph(project: string | null) {
    const q = project ? `?project=${encodeURIComponent(project)}` : '';
    return getJson<GraphResponse>(`/graph${q}`);
  },
  projects() {
    return getJson<ProjectSummary[]>('/projects');
  },
  session(id: string) {
    return getJson<SessionRecord>(`/sessions/${encodeURIComponent(id)}`);
  },
  transcript(id: string) {
    return getJson<TranscriptEntry[]>(
      `/sessions/${encodeURIComponent(id)}/transcript`,
    );
  },
};

export interface Health {
  status: 'ok' | 'indexing';
  sessions: number;
  chunks: number;
  indexing: boolean;
  scan_progress: number;
}

export interface RecallItem {
  chunk_id: string;
  session_id: string;
  project: string;
  score: number;
  text: string;
  token_count: number;
  session_title: string | null;
  started_at: string;
  /** C4 provenance: short human reason this chunk was recalled (e.g.
   *  "semantic 0.71 · keywords: duckdb, schema"). */
  why: string;
}

/** C4: a distilled memory injected into a recall response (separate from
 *  `items`). `why` explains the injection tier. */
export interface RecallMemory {
  kind: 'memory';
  id: string;
  project: string;
  content: string;
  source: MemorySource;
  pinned: boolean;
  scope: MemoryScope;
  globs?: string[] | null;
  score: number;
  /** C4 provenance: e.g. "standing rule (always)", a "matches glob" phrase
   *  naming the matched pattern, or "semantic match 0.83". */
  why: string;
}

export interface RecallResponse {
  items: RecallItem[];
  /** Distilled memories matching the query+project; additive (may be absent on
   *  older daemons). */
  memories?: RecallMemory[];
  total_chunks: number;
  total_tokens: number;
  truncated: boolean;
  elapsed_ms: number;
}

export interface SessionSummary {
  id: string;
  project_id: string;
  is_sidechain: boolean;
  started_at: string;
  ended_at: string;
  message_count: number;
  ai_title: string | null;
  first_prompt: string | null;
  chunk_count: number;
}

export interface HookSettings {
  score_threshold: number;
  limit: number;
  token_budget: number;
  min_words: number;
  scope: 'project' | 'global';
}

export interface Settings {
  auto_promote: boolean;
  hook: HookSettings;
}

async function sendJson<T>(path: string, method: string, body: unknown): Promise<T> {
  const r = await fetch(`${BASE}${path}`, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    // C4: send the same-origin `ck_token` cookie (see getJson). Explicit even
    // though it's the default, so the auth contract is visible at the call site.
    credentials: 'same-origin',
  });
  if (!r.ok) {
    let msg = `${r.status} ${r.statusText}`;
    try {
      const j = (await r.json()) as { error?: string };
      if (j.error) msg = j.error;
    } catch {
      /* keep status text */
    }
    throw new Error(msg);
  }
  return r.json() as Promise<T>;
}

// ---- /v1/memories (C1 writable store + C2 injection scope) ----
// Wire shape mirrors ck_store::Memory's serde(Serialize) derive (default
// snake_case, no rename_all). Timestamps are unix SECONDS (i64). `globs` is
// null for every scope except "glob". `source`/`scope` are open strings on the
// wire but only the documented variants are ever produced/accepted.

export type MemorySource = 'agent' | 'user' | 'distilled';
export type MemoryScope = 'auto' | 'always' | 'glob' | 'manual';

export interface Memory {
  id: string;
  project_id: string;
  content: string;
  source: MemorySource;
  pinned: boolean;
  scope: MemoryScope;
  globs: string[] | null;
  created_at: number;
  updated_at: number;
}

/** The reserved project id under which `global:true` memories are stored. */
export const GLOBAL_PROJECT = '__global__';

export interface CreateMemoryBody {
  /** Required unless `global:true`; ignored when `global` is set. */
  project_id?: string;
  /** Store under the reserved `__global__` project (a rule for every project). */
  global?: boolean;
  content: string;
  source?: MemorySource;
  pinned?: boolean;
  scope?: MemoryScope;
  /** Required (non-empty) when scope==='glob'; ignored otherwise. */
  globs?: string[];
}

export interface UpdateMemoryBody {
  content?: string;
  pinned?: boolean;
  scope?: MemoryScope;
  globs?: string[];
}

export interface ListMemoriesOpts {
  limit?: number;
  scope?: MemoryScope;
}

export const apiExtra = {
  health() {
    return getJson<Health>('/health');
  },
  /**
   * GET /v1/memories — `project` is REQUIRED by the API (single-project query;
   * pass GLOBAL_PROJECT for global rules). Server returns pinned-first,
   * then most-recently-updated.
   */
  listMemories(project: string, opts: ListMemoriesOpts = {}) {
    const params = new URLSearchParams({ project });
    if (opts.limit != null) params.set('limit', String(opts.limit));
    if (opts.scope) params.set('scope', opts.scope);
    return getJson<Memory[]>(`/memories?${params.toString()}`);
  },
  /** POST /v1/memories. 400 (e.g. glob-without-globs) surfaces the server msg. */
  createMemory(body: CreateMemoryBody) {
    return sendJson<Memory>('/memories', 'POST', body);
  },
  /** PUT /v1/memories/:id. 404 / 400 surface the server's error message. */
  updateMemory(id: string, body: UpdateMemoryBody) {
    return sendJson<Memory>(`/memories/${encodeURIComponent(id)}`, 'PUT', body);
  },
  /** DELETE /v1/memories/:id → { ok, deleted }. 404 surfaces the server msg. */
  deleteMemory(id: string) {
    return sendJson<{ ok: boolean; deleted: string }>(
      `/memories/${encodeURIComponent(id)}`,
      'DELETE',
      undefined,
    );
  },
  sessions(project: string | null, limit = 1000) {
    const params = new URLSearchParams({ limit: String(limit) });
    if (project) params.set('project', project);
    return getJson<SessionSummary[]>(`/sessions?${params.toString()}`);
  },
  recall(query: string, project: string | null) {
    return sendJson<RecallResponse>('/recall', 'POST', {
      query,
      limit: 12,
      token_budget: 6000,
      source: 'http',
      ...(project ? { project } : {}),
    });
  },
  getSettings() {
    return getJson<Settings>('/settings');
  },
  putSettings(s: Settings) {
    return sendJson<{ ok: boolean; path: string }>('/settings', 'PUT', s);
  },
};

/** ws:// URL for /v1/ws on the same origin the UI was served from. */
export function wsUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  return `${proto}://${location.host}${BASE}/ws`;
}
