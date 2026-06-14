// The primary view: a practical, search-first session browser.
//
// Left rail = projects (filter). Main = a scannable list of sessions, newest
// first, with instant title filtering. Clicking a row opens the full session;
// "Search content" hands off to semantic recall (⌘K) for what-was-said search.

import { useMemo, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import {
  Search,
  X,
  Layers,
  MessageSquare,
  Bot,
  Loader2,
  Sparkles,
} from 'lucide-react';
import { apiExtra, type SessionSummary, type ProjectSummary } from '../api';
import { prettyProject, hueFor, pastelBg, pastelBorder, pastelText } from '../visual';
import SessionPanel from './SessionPanel';

interface Props {
  projects: ProjectSummary[];
  selectedProject: string | null;
  onSelectProject: (id: string | null) => void;
  onSearchContent: (q: string) => void;
}

function relTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return '';
  const s = Math.max(0, (Date.now() - then) / 1000);
  if (s < 60) return 'just now';
  const m = s / 60;
  if (m < 60) return `${Math.floor(m)}m ago`;
  const h = m / 60;
  if (h < 24) return `${Math.floor(h)}h ago`;
  const d = h / 24;
  if (d < 7) return `${Math.floor(d)}d ago`;
  const w = d / 7;
  if (w < 5) return `${Math.floor(w)}w ago`;
  const mo = d / 30;
  if (mo < 12) return `${Math.floor(mo)}mo ago`;
  return `${Math.floor(d / 365)}y ago`;
}

export default function Browser({
  projects,
  selectedProject,
  onSelectProject,
  onSearchContent,
}: Props) {
  const [filter, setFilter] = useState('');
  const [openSession, setOpenSession] = useState<string | null>(null);

  const sessions = useQuery({
    queryKey: ['sessions', selectedProject],
    queryFn: () => apiExtra.sessions(selectedProject, 1000),
    refetchInterval: 60_000,
  });

  const sortedProjects = useMemo(
    () =>
      [...projects].sort((a, b) =>
        (b.last_seen ?? '').localeCompare(a.last_seen ?? ''),
      ),
    [projects],
  );
  const totalSessions = useMemo(
    () => projects.reduce((a, p) => a + p.sessions, 0),
    [projects],
  );

  const rows = useMemo(() => {
    const list = sessions.data ?? [];
    const sorted = [...list].sort((a, b) =>
      b.ended_at.localeCompare(a.ended_at),
    );
    const q = filter.trim().toLowerCase();
    if (!q) return sorted;
    return sorted.filter((s) => {
      const title = (s.ai_title ?? s.first_prompt ?? s.id).toLowerCase();
      const prompt = (s.first_prompt ?? '').toLowerCase();
      return title.includes(q) || prompt.includes(q);
    });
  }, [sessions.data, filter]);

  return (
    <div className="absolute inset-0 flex">
      {/* Project rail */}
      <aside className="w-[230px] shrink-0 border-r border-ink-700/50 bg-ink-900/40 flex flex-col">
        <div className="px-3 py-2.5 text-[10px] uppercase tracking-widest font-bold text-ink-300 border-b border-ink-800/60">
          Projects
        </div>
        <div className="flex-1 overflow-y-auto ck-scroll py-1.5">
          <RailRow
            active={selectedProject === null}
            onClick={() => onSelectProject(null)}
            dot={null}
            label="All projects"
            count={totalSessions}
            icon={<Layers className="w-3.5 h-3.5" />}
          />
          {sortedProjects.map((p) => (
            <RailRow
              key={p.id}
              active={selectedProject === p.id}
              onClick={() => onSelectProject(p.id)}
              dot={`hsl(${hueFor(p.id)}, 70%, 55%)`}
              label={prettyProject(p.id)}
              count={p.sessions}
              title={p.id}
            />
          ))}
        </div>
      </aside>

      {/* Session list */}
      <div className="flex-1 min-w-0 flex flex-col">
        <div className="px-4 py-3 border-b border-ink-700/50 flex items-center gap-2.5">
          <div className="relative flex-1">
            <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-ink-300" />
            <input
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder={`Filter ${rows.length} session${rows.length === 1 ? '' : 's'} by title…`}
              className="w-full bg-ink-800/60 border border-ink-700/70 rounded-lg pl-9 pr-9 py-2 text-[13px] text-ink-50 placeholder:text-ink-300/70 outline-none focus:border-accent/50"
            />
            {filter && (
              <button
                onClick={() => setFilter('')}
                className="absolute right-2.5 top-1/2 -translate-y-1/2 text-ink-300 hover:text-ink-50"
                aria-label="clear filter"
              >
                <X className="w-3.5 h-3.5" />
              </button>
            )}
          </div>
          <button
            onClick={() => onSearchContent(filter)}
            title="Search what was said inside chats (semantic) — ⌘K"
            className="flex items-center gap-1.5 text-[12px] px-3 py-2 rounded-lg bg-accent/12 border border-accent/35 text-accent hover:bg-accent/20 transition-colors shrink-0"
          >
            <Sparkles className="w-3.5 h-3.5" />
            <span>Search content</span>
          </button>
        </div>

        <div className="flex-1 overflow-y-auto ck-scroll">
          {sessions.isLoading && (
            <div className="px-4 py-6 text-[12.5px] text-ink-300 flex items-center gap-2">
              <Loader2 className="w-4 h-4 animate-spin" /> loading sessions…
            </div>
          )}
          {sessions.data && rows.length === 0 && (
            <div className="px-4 py-12 text-center text-[12.5px] text-ink-300">
              {filter
                ? `No sessions match “${filter}”.`
                : 'No sessions indexed yet.'}
              {filter && (
                <div className="mt-2">
                  <button
                    onClick={() => onSearchContent(filter)}
                    className="text-accent hover:underline"
                  >
                    Search the content of your chats for “{filter}” →
                  </button>
                </div>
              )}
            </div>
          )}
          {rows.map((s) => (
            <SessionRow
              key={s.id}
              s={s}
              onOpen={() => setOpenSession(s.id)}
            />
          ))}
        </div>
      </div>

      <SessionPanel
        sessionId={openSession}
        onClose={() => setOpenSession(null)}
      />
    </div>
  );
}

function RailRow({
  active,
  onClick,
  dot,
  label,
  count,
  icon,
  title,
}: {
  active: boolean;
  onClick: () => void;
  dot: string | null;
  label: string;
  count: number;
  icon?: React.ReactNode;
  title?: string;
}) {
  return (
    <button
      onClick={onClick}
      title={title}
      className={`w-full flex items-center gap-2 px-3 py-1.5 text-[12.5px] transition-colors ${
        active
          ? 'bg-accent/12 text-ink-50'
          : 'text-ink-200 hover:bg-ink-800/50'
      }`}
    >
      {dot ? (
        <span
          className="w-2 h-2 rounded-full shrink-0"
          style={{ background: dot }}
        />
      ) : (
        <span className="text-ink-300 shrink-0">{icon}</span>
      )}
      <span className="truncate flex-1 text-left">{label}</span>
      <span className="text-[10.5px] font-mono text-ink-300 shrink-0">
        {count}
      </span>
    </button>
  );
}

function SessionRow({ s, onOpen }: { s: SessionSummary; onOpen: () => void }) {
  const title = s.ai_title ?? s.first_prompt ?? s.id.slice(0, 18);
  const preview =
    s.ai_title && s.first_prompt && s.first_prompt !== s.ai_title
      ? s.first_prompt
      : null;
  return (
    <button
      onClick={onOpen}
      className="w-full text-left px-4 py-2.5 border-b border-ink-800/40 hover:bg-ink-800/40 transition-colors flex gap-3 group"
    >
      <span
        className={`grid place-items-center w-7 h-7 mt-0.5 rounded-lg shrink-0 border ${
          s.is_sidechain
            ? 'bg-project/15 border-project/40 text-project'
            : 'bg-accent/12 border-accent/30 text-accent'
        }`}
      >
        {s.is_sidechain ? (
          <Bot className="w-3.5 h-3.5" />
        ) : (
          <MessageSquare className="w-3.5 h-3.5" />
        )}
      </span>
      <div className="min-w-0 flex-1">
        <div className="text-[13px] text-ink-50 font-medium truncate group-hover:text-accent transition-colors">
          {title}
        </div>
        {preview && (
          <div className="text-[11.5px] text-ink-300 truncate mt-0.5">
            {preview}
          </div>
        )}
        <div className="flex items-center gap-2 mt-1 text-[10.5px] text-ink-300/80 font-mono">
          <span
            className="px-1.5 py-0.5 rounded"
            style={{
              background: pastelBg(s.project_id, 0.16),
              border: `1px solid ${pastelBorder(s.project_id, 0.35)}`,
              color: pastelText(s.project_id),
            }}
          >
            {prettyProject(s.project_id)}
          </span>
          <span title={s.ended_at}>{relTime(s.ended_at)}</span>
          <span className="opacity-50">·</span>
          <span>{s.message_count} msgs</span>
          <span className="opacity-50">·</span>
          <span>{s.chunk_count} chunks</span>
          {s.is_sidechain && (
            <span className="text-project-soft">· subagent</span>
          )}
        </div>
      </div>
    </button>
  );
}
