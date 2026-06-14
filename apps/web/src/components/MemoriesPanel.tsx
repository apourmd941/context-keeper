// Memories panel: view / pin / edit / delete / create the writable memory
// store (C1) with per-memory injection scope (C2), over /v1/memories*.
//
// Mirrors RecallPanel's right-side overlay shell and SettingsPanel's form
// idioms (selran-theme tokens, emerald accent, dark/light). The current
// project's memories and the reserved `__global__` rules are listed together
// (two GETs merged), pinned-first then most-recently-updated. Memory content
// is always rendered as TEXT — never dangerouslySetInnerHTML — so a memory
// holding HTML/script can't inject.

import { useEffect, useMemo, useRef, useState } from 'react';
import {
  useMutation,
  useQueries,
  useQueryClient,
} from '@tanstack/react-query';
import { AnimatePresence, motion } from 'framer-motion';
import {
  Brain,
  X,
  Pin,
  Globe,
  Pencil,
  Trash2,
  Plus,
  Check,
  Loader2,
  Sparkles,
} from 'lucide-react';
import {
  apiExtra,
  GLOBAL_PROJECT,
  type Memory,
  type MemoryScope,
  type CreateMemoryBody,
  type UpdateMemoryBody,
} from '../api';
import { prettyProject } from '../visual';

interface Props {
  open: boolean;
  project: string | null;
  onClose: () => void;
}

const SCOPES: MemoryScope[] = ['auto', 'always', 'glob', 'manual'];
type ScopeFilter = 'all' | MemoryScope;

// Short human label for what each scope does, used in selects/help text.
const SCOPE_HINT: Record<MemoryScope, string> = {
  auto: 'injected on a semantic match',
  always: 'a standing rule — injected on every recall',
  glob: 'injected only for matching file paths',
  manual: 'never auto-injected; surfaced here only',
};

export default function MemoriesPanel({ open, project, onClose }: Props) {
  const qc = useQueryClient();
  const [scopeFilter, setScopeFilter] = useState<ScopeFilter>('all');
  const [includeGlobal, setIncludeGlobal] = useState(true);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);

  // Esc closes the panel (consistent with Recall/Settings). When a sub-form
  // (create/edit) is open, Esc closes that first so it's a graceful step-back.
  // App.tsx's global Esc handler also fires; closing the panel is idempotent.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      if (creating || editing) {
        e.stopPropagation();
        setCreating(false);
        setEditing(null);
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [open, creating, editing]);

  // Reset transient form state whenever the panel is closed.
  useEffect(() => {
    if (!open) {
      setCreating(false);
      setEditing(null);
    }
  }, [open]);

  // Two independent reads: the current project, and the global rules. The API
  // requires a single `project` per call, so we merge client-side. Global is
  // skipped when toggled off (or when there's no project the global read still
  // stands on its own).
  const wantProject = project != null;
  const wantGlobal = includeGlobal;
  const scopeOpt = scopeFilter === 'all' ? undefined : scopeFilter;

  const results = useQueries({
    queries: [
      {
        queryKey: ['memories', project, scopeFilter],
        queryFn: () => apiExtra.listMemories(project as string, { scope: scopeOpt }),
        enabled: open && wantProject,
      },
      {
        queryKey: ['memories', GLOBAL_PROJECT, scopeFilter],
        queryFn: () =>
          apiExtra.listMemories(GLOBAL_PROJECT, { scope: scopeOpt }),
        enabled: open && wantGlobal,
      },
    ],
  });

  const [projQuery, globalQuery] = results;
  const loading =
    (wantProject && projQuery.isLoading) ||
    (wantGlobal && globalQuery.isLoading);
  const errorMsg =
    (wantProject && projQuery.error
      ? (projQuery.error as Error).message
      : null) ??
    (wantGlobal && globalQuery.error
      ? (globalQuery.error as Error).message
      : null);

  const memories = useMemo(() => {
    const rows: Memory[] = [];
    if (wantProject && projQuery.data) rows.push(...projQuery.data);
    if (wantGlobal && globalQuery.data) rows.push(...globalQuery.data);
    // Dedupe (a project of `__global__` would otherwise appear twice if both
    // reads ran), then sort pinned-first, newest-updated next — matching the
    // server's per-project ordering across the merged set.
    const seen = new Set<string>();
    const deduped = rows.filter((m) => {
      if (seen.has(m.id)) return false;
      seen.add(m.id);
      return true;
    });
    deduped.sort((a, b) => {
      if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
      return b.updated_at - a.updated_at;
    });
    return deduped;
  }, [wantProject, wantGlobal, projQuery.data, globalQuery.data]);

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ['memories'] });
  };

  return (
    <AnimatePresence>
      {open && (
        <motion.aside
          role="dialog"
          aria-label="Memories"
          initial={{ x: 560, opacity: 0 }}
          animate={{ x: 0, opacity: 1 }}
          exit={{ x: 560, opacity: 0 }}
          transition={{ type: 'spring', stiffness: 230, damping: 28 }}
          className="absolute right-0 top-0 bottom-0 w-[540px] z-20 flex flex-col bg-ink-900/95 backdrop-blur-md border-l border-ink-700/60 shadow-2xl"
        >
          {/* Header */}
          <div className="px-4 py-3 border-b border-ink-700/60 flex items-center gap-2 bg-gradient-to-br from-ink-800/80 to-ink-900/80">
            <Brain className="w-4 h-4 text-accent" />
            <span className="font-bold text-ink-50 text-[14px]">Memories</span>
            <span className="text-[10px] text-ink-300/70 font-mono ml-0.5">
              {project ? prettyProject(project) : 'all projects'}
            </span>
            <button
              onClick={() => {
                setEditing(null);
                setCreating((v) => !v);
              }}
              className="ml-auto flex items-center gap-1.5 text-[12px] font-semibold px-3 py-1.5 rounded-lg bg-accent text-accent-contrast hover:brightness-110 transition"
            >
              <Plus className="w-3.5 h-3.5" />
              add
            </button>
            <button
              onClick={onClose}
              className="grid place-items-center w-8 h-8 rounded-xl bg-ink-800/80 border border-ink-700/60 text-ink-300 hover:text-ink-50 transition-colors"
              aria-label="close memories"
            >
              <X className="w-4 h-4" />
            </button>
          </div>

          {/* Filter bar */}
          <div className="px-4 py-2.5 border-b border-ink-800/60 flex items-center gap-2 flex-wrap">
            <div
              className="flex items-center gap-1"
              role="group"
              aria-label="filter by scope"
            >
              {(['all', ...SCOPES] as ScopeFilter[]).map((s) => (
                <button
                  key={s}
                  onClick={() => setScopeFilter(s)}
                  aria-pressed={scopeFilter === s}
                  className={`px-2.5 py-1 rounded-lg text-[11px] font-medium border transition-colors ${
                    scopeFilter === s
                      ? 'bg-accent/15 border-accent/50 text-accent'
                      : 'bg-ink-800/50 border-ink-700/60 text-ink-300 hover:text-ink-100'
                  }`}
                >
                  {s}
                </button>
              ))}
            </div>
            <label className="ml-auto flex items-center gap-1.5 text-[11px] text-ink-300 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={includeGlobal}
                onChange={(e) => setIncludeGlobal(e.target.checked)}
                className="accent-[var(--accent)]"
              />
              include global
            </label>
          </div>

          {/* Create form */}
          <AnimatePresence>
            {creating && (
              <motion.div
                initial={{ height: 0, opacity: 0 }}
                animate={{ height: 'auto', opacity: 1 }}
                exit={{ height: 0, opacity: 0 }}
                className="overflow-hidden border-b border-ink-800/60 bg-ink-800/30"
              >
                <MemoryForm
                  mode="create"
                  project={project}
                  onClose={() => setCreating(false)}
                  onSaved={() => {
                    setCreating(false);
                    refresh();
                  }}
                />
              </motion.div>
            )}
          </AnimatePresence>

          {/* List */}
          <div className="flex-1 min-h-0 overflow-y-auto ck-scroll">
            {errorMsg && (
              <div className="px-4 py-3 text-[12px] text-[color:var(--danger)]">
                {errorMsg}
              </div>
            )}
            {loading && (
              <div className="px-4 py-8 text-center text-[12.5px] text-ink-300 flex items-center justify-center gap-2">
                <Loader2 className="w-4 h-4 animate-spin" /> loading memories…
              </div>
            )}
            {!loading && !errorMsg && memories.length === 0 && (
              <div className="px-4 py-10 text-center text-[12.5px] text-ink-300/80 leading-relaxed">
                No memories yet — the agent writes these with{' '}
                <span className="font-mono text-ink-200">remember</span>, or add
                one here.
              </div>
            )}
            {!loading &&
              memories.map((m) =>
                editing === m.id ? (
                  <div
                    key={m.id}
                    className="border-b border-ink-800/50 bg-ink-800/30"
                  >
                    <MemoryForm
                      mode="edit"
                      memory={m}
                      project={project}
                      onClose={() => setEditing(null)}
                      onSaved={() => {
                        setEditing(null);
                        refresh();
                      }}
                    />
                  </div>
                ) : (
                  <MemoryRow
                    key={m.id}
                    memory={m}
                    onEdit={() => {
                      setCreating(false);
                      setEditing(m.id);
                    }}
                    onChanged={refresh}
                  />
                ),
              )}
          </div>
        </motion.aside>
      )}
    </AnimatePresence>
  );
}

// ---- A single memory row (read mode) ----

function MemoryRow({
  memory,
  onEdit,
  onChanged,
}: {
  memory: Memory;
  onEdit: () => void;
  onChanged: () => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState(false);
  const isGlobal = memory.project_id === GLOBAL_PROJECT;

  const pin = useMutation({
    mutationFn: () =>
      apiExtra.updateMemory(memory.id, { pinned: !memory.pinned }),
    onSuccess: onChanged,
  });
  const del = useMutation({
    mutationFn: () => apiExtra.deleteMemory(memory.id),
    onSuccess: onChanged,
  });

  const err =
    (pin.error as Error | null)?.message ??
    (del.error as Error | null)?.message ??
    null;

  return (
    <div className="px-4 py-3 border-b border-ink-800/50 hover:bg-ink-800/30 transition-colors group">
      <div className="flex items-center gap-2 mb-1.5">
        <ScopeBadge scope={memory.scope} />
        {isGlobal && (
          <span
            className="inline-flex items-center gap-1 text-[10px] px-1.5 py-0.5 rounded-md bg-ink-800/70 border border-ink-700/60 text-ink-300"
            title="applies to every project"
          >
            <Globe className="w-2.5 h-2.5" />
            global
          </span>
        )}
        <span className="text-[10px] text-ink-300/70 font-mono">
          {memory.source}
        </span>
        <span
          className="ml-auto text-[10px] text-ink-300/60 font-mono"
          title={new Date(memory.updated_at * 1000).toLocaleString()}
        >
          {relativeTime(memory.updated_at)}
        </span>
        <button
          onClick={() => pin.mutate()}
          disabled={pin.isPending}
          aria-pressed={memory.pinned}
          aria-label={memory.pinned ? 'unpin memory' : 'pin memory'}
          title={memory.pinned ? 'unpin' : 'pin (bypasses the recall floor)'}
          className={`grid place-items-center w-6 h-6 rounded-md transition-colors ${
            memory.pinned
              ? 'text-accent'
              : 'text-ink-300/50 hover:text-ink-100'
          }`}
        >
          <Pin
            className="w-3.5 h-3.5"
            fill={memory.pinned ? 'currentColor' : 'none'}
          />
        </button>
      </div>

      {/* TEXT only — never dangerouslySetInnerHTML (React escapes this). */}
      <p className="text-[12.5px] leading-snug text-ink-100/90 whitespace-pre-wrap break-words">
        {memory.content}
      </p>

      {memory.scope === 'glob' && memory.globs && memory.globs.length > 0 && (
        <div className="mt-1.5 flex flex-wrap gap-1">
          {memory.globs.map((g) => (
            <span
              key={g}
              className="text-[10px] font-mono px-1.5 py-0.5 rounded-md bg-ink-800/70 border border-ink-700/60 text-ink-200"
            >
              {g}
            </span>
          ))}
        </div>
      )}

      {/* C4: a calm "why it injects" caption mirroring recall provenance, for
          the scopes where the injection reason is non-obvious (always/glob).
          auto/manual read clearly from the badge alone. */}
      {(memory.scope === 'always' || memory.scope === 'glob') && (
        <p className="mt-1 text-[10.5px] italic text-ink-300/70">
          {SCOPE_HINT[memory.scope]}
        </p>
      )}

      {err && (
        <div className="mt-1.5 text-[11px] text-[color:var(--danger)]">
          {err}
        </div>
      )}

      <div className="mt-2 flex items-center gap-2 opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity">
        <button
          onClick={onEdit}
          className="flex items-center gap-1 text-[11px] text-ink-300 hover:text-ink-100 transition-colors"
        >
          <Pencil className="w-3 h-3" /> edit
        </button>
        {confirmDelete ? (
          <span className="flex items-center gap-1.5 text-[11px]">
            <span className="text-ink-300">delete?</span>
            <button
              onClick={() => del.mutate()}
              disabled={del.isPending}
              className="font-semibold text-[color:var(--danger)] hover:brightness-110 disabled:opacity-50"
            >
              {del.isPending ? 'deleting…' : 'yes'}
            </button>
            <button
              onClick={() => setConfirmDelete(false)}
              className="text-ink-300 hover:text-ink-100"
            >
              cancel
            </button>
          </span>
        ) : (
          <button
            onClick={() => setConfirmDelete(true)}
            className="flex items-center gap-1 text-[11px] text-ink-300 hover:text-[color:var(--danger)] transition-colors"
          >
            <Trash2 className="w-3 h-3" /> delete
          </button>
        )}
      </div>
    </div>
  );
}

// ---- Create / edit form ----

function MemoryForm({
  mode,
  memory,
  project,
  onClose,
  onSaved,
}: {
  mode: 'create' | 'edit';
  memory?: Memory;
  project: string | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [content, setContent] = useState(memory?.content ?? '');
  const [scope, setScope] = useState<MemoryScope>(memory?.scope ?? 'auto');
  const [globsText, setGlobsText] = useState(
    (memory?.globs ?? []).join(', '),
  );
  const [pinned, setPinned] = useState(memory?.pinned ?? false);
  // Create-only: whether this is a global rule. In edit mode a memory's
  // project is fixed (the API has no move), so the toggle is hidden.
  const [global, setGlobal] = useState(
    memory?.project_id === GLOBAL_PROJECT,
  );
  const [localErr, setLocalErr] = useState<string | null>(null);

  const firstFieldRef = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    firstFieldRef.current?.focus();
  }, []);

  const parseGlobs = () =>
    globsText
      .split(/[\n,]/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);

  const save = useMutation({
    mutationFn: () => {
      if (mode === 'create') {
        const body: CreateMemoryBody = {
          content: content.trim(),
          scope,
          pinned,
          source: 'user',
        };
        if (global) body.global = true;
        else if (project) body.project_id = project;
        if (scope === 'glob') body.globs = parseGlobs();
        return apiExtra.createMemory(body);
      }
      const body: UpdateMemoryBody = {
        content: content.trim(),
        scope,
        pinned,
      };
      if (scope === 'glob') body.globs = parseGlobs();
      return apiExtra.updateMemory(memory!.id, body);
    },
    onSuccess: onSaved,
  });

  const submit = () => {
    setLocalErr(null);
    if (content.trim().length === 0) {
      setLocalErr('Content must not be empty.');
      return;
    }
    if (mode === 'create' && !global && !project) {
      setLocalErr('No project selected — turn on “global rule” to save.');
      return;
    }
    // Mirror the server's scope=glob contract so the user gets the message
    // before the round-trip (the API would 400 anyway).
    if (scope === 'glob' && parseGlobs().length === 0) {
      setLocalErr('Glob scope needs at least one pattern (e.g. **/*.ts).');
      return;
    }
    // Mirror the server's byte/count bounds (UTF-8 bytes, not UTF-16 units) so
    // the user sees the message before the round-trip — the API enforces these
    // and would 400 otherwise. Pattern *syntax* stays the server's authority.
    if (new TextEncoder().encode(content.trim()).length > 8192) {
      setLocalErr('Content too long (max 8192 bytes).');
      return;
    }
    if (scope === 'glob') {
      const globs = parseGlobs();
      if (globs.length > 32) {
        setLocalErr('At most 32 glob patterns.');
        return;
      }
      if (globs.some((g) => new TextEncoder().encode(g).length > 256)) {
        setLocalErr('Each glob pattern must be ≤ 256 bytes.');
        return;
      }
    }
    save.mutate();
  };

  const serverErr = save.error ? (save.error as Error).message : null;

  return (
    <div className="px-4 py-3 space-y-3">
      <label className="block">
        <span className="block text-[11px] text-ink-300 mb-1">
          {mode === 'create' ? 'New memory' : 'Content'}
        </span>
        <textarea
          ref={firstFieldRef}
          value={content}
          onChange={(e) => setContent(e.target.value)}
          rows={3}
          placeholder="e.g. Always run the formatter before committing."
          className="w-full bg-ink-800/70 border border-ink-700/60 rounded-xl px-3 py-2 text-[12.5px] text-ink-50 outline-none focus:border-accent/60 resize-y whitespace-pre-wrap"
        />
      </label>

      <div className="flex items-end gap-3 flex-wrap">
        <label className="block">
          <span className="block text-[11px] text-ink-300 mb-1">scope</span>
          <select
            value={scope}
            onChange={(e) => setScope(e.target.value as MemoryScope)}
            className="bg-ink-800/70 border border-ink-700/60 rounded-xl px-2.5 py-1.5 text-[12px] text-ink-50 outline-none focus:border-accent/60"
          >
            {SCOPES.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>

        <label className="flex items-center gap-1.5 text-[11.5px] text-ink-200 cursor-pointer select-none pb-1.5">
          <input
            type="checkbox"
            checked={pinned}
            onChange={(e) => setPinned(e.target.checked)}
            className="accent-[var(--accent)]"
          />
          <Pin className="w-3 h-3" /> pinned
        </label>

        {mode === 'create' && (
          <label className="flex items-center gap-1.5 text-[11.5px] text-ink-200 cursor-pointer select-none pb-1.5">
            <input
              type="checkbox"
              checked={global}
              onChange={(e) => setGlobal(e.target.checked)}
              className="accent-[var(--accent)]"
            />
            <Globe className="w-3 h-3" /> global rule
          </label>
        )}
      </div>

      <p className="text-[10.5px] text-ink-300/70 leading-snug flex items-start gap-1">
        <Sparkles className="w-3 h-3 mt-0.5 shrink-0 text-accent/70" />
        <span>{SCOPE_HINT[scope]}.</span>
      </p>

      {scope === 'glob' && (
        <label className="block">
          <span className="block text-[11px] text-ink-300 mb-1">
            globs <span className="text-ink-300/60">(comma or newline)</span>
          </span>
          <input
            value={globsText}
            onChange={(e) => setGlobsText(e.target.value)}
            placeholder="**/*.ts, src/**/*.rs"
            className="w-full bg-ink-800/70 border border-ink-700/60 rounded-xl px-3 py-1.5 text-[12px] font-mono text-ink-50 outline-none focus:border-accent/60"
          />
        </label>
      )}

      {(localErr || serverErr) && (
        <div className="text-[11.5px] text-[color:var(--danger)]">
          {localErr ?? serverErr}
        </div>
      )}

      <div className="flex justify-end gap-2 pt-0.5">
        <button
          onClick={onClose}
          className="px-3 py-1.5 rounded-xl text-[12px] text-ink-300 hover:text-ink-100 transition-colors"
        >
          cancel
        </button>
        <button
          onClick={submit}
          disabled={save.isPending}
          className="flex items-center gap-1.5 px-4 py-1.5 rounded-xl text-[12px] font-bold bg-accent text-accent-contrast disabled:opacity-50 hover:brightness-110 transition"
        >
          {save.isPending ? (
            <Loader2 className="w-3.5 h-3.5 animate-spin" />
          ) : (
            <Check className="w-3.5 h-3.5" />
          )}
          {mode === 'create' ? 'create' : 'save'}
        </button>
      </div>
    </div>
  );
}

// ---- Scope badge: calm data, not a loud chip. Each scope gets a steady,
// theme-aware tint drawn from the shared token palette (no hard-coded hex). ----

function ScopeBadge({ scope }: { scope: MemoryScope }) {
  // Map each scope to a semantic token so the badge stays calm and on-theme
  // in both dark and light. `always` (the headline rule) leans on the accent;
  // the rest use neutral/role tokens.
  const tint: Record<MemoryScope, { color: string; soft: string }> = {
    always: { color: 'var(--accent)', soft: 'var(--accent-soft)' },
    auto: { color: 'var(--info)', soft: 'color-mix(in srgb, var(--info) 16%, transparent)' },
    glob: { color: 'var(--success)', soft: 'color-mix(in srgb, var(--success) 16%, transparent)' },
    manual: { color: 'var(--fg-muted)', soft: 'color-mix(in srgb, var(--fg-muted) 14%, transparent)' },
  };
  const { color, soft } = tint[scope];
  return (
    <span
      className="text-[10px] px-2 py-0.5 rounded-md font-semibold tracking-tight"
      style={{
        color,
        background: soft,
        border: `1px solid color-mix(in srgb, ${color} 35%, transparent)`,
      }}
      title={SCOPE_HINT[scope]}
    >
      {scope}
    </span>
  );
}

// Compact relative time ("3m", "5h", "2d") from a unix-seconds timestamp.
function relativeTime(unixSeconds: number): string {
  const diff = Date.now() / 1000 - unixSeconds;
  if (diff < 60) return 'just now';
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 2592000) return `${Math.floor(diff / 86400)}d ago`;
  return new Date(unixSeconds * 1000).toLocaleDateString();
}
