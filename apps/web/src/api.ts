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
  const r = await fetch(`${BASE}${path}`);
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
}

export interface RecallResponse {
  items: RecallItem[];
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

export const apiExtra = {
  health() {
    return getJson<Health>('/health');
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
