// Top-level shell: brand + view toggle (Browser primary / Graph secondary),
// recall panel, settings, theme cycle (dark/light/system), and a live
// indexing pill. View + theme persist; ⌘K opens recall, Esc closes overlays.

import { useEffect, useMemo, useRef, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Share2,
  List,
  Search,
  Settings as SettingsIcon,
  Loader2,
  Waypoints,
  Sun,
  Moon,
  Monitor,
} from 'lucide-react';
import { api, apiExtra, wsUrl } from './api';
import Browser from './components/Browser';
import KnowledgeGraph from './components/KnowledgeGraph';
import RecallPanel from './components/RecallPanel';
import SettingsPanel from './components/SettingsPanel';

type View = 'browser' | 'graph';
type ThemePref = 'system' | 'dark' | 'light';
const VIEW_KEY = 'ck:view';
const THEME_KEY = 'ck:theme';

function resolveTheme(pref: ThemePref): 'dark' | 'light' {
  if (pref !== 'system') return pref;
  return window.matchMedia('(prefers-color-scheme: light)').matches
    ? 'light'
    : 'dark';
}

export default function App() {
  const qc = useQueryClient();
  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: api.projects,
    refetchInterval: 60_000,
  });
  const health = useQuery({
    queryKey: ['health'],
    queryFn: apiExtra.health,
    refetchInterval: (q) => (q.state.data?.indexing ? 1_500 : 30_000),
  });

  const params =
    typeof window !== 'undefined'
      ? new URLSearchParams(window.location.search)
      : null;
  const [recallOpen, setRecallOpen] = useState(params?.get('recall') === '1');
  const [recallSeed, setRecallSeed] = useState('');
  const [settingsOpen, setSettingsOpen] = useState(
    params?.get('settings') === '1',
  );
  // Project filter shared by the browser rail and recall's scope.
  const [project, setProject] = useState<string | null>(null);
  const [view, setView] = useState<View>(() => {
    const fromUrl = params?.get('view');
    if (fromUrl === 'browser' || fromUrl === 'graph') return fromUrl;
    const stored = localStorage.getItem(VIEW_KEY);
    // Browser is the primary, practical view; Graph is the secondary visual.
    return stored === 'browser' || stored === 'graph' ? stored : 'browser';
  });
  const [themePref, setThemePref] = useState<ThemePref>(() => {
    const fromUrl = params?.get('theme');
    if (fromUrl === 'dark' || fromUrl === 'light' || fromUrl === 'system') {
      localStorage.setItem(THEME_KEY, fromUrl);
      return fromUrl;
    }
    const stored = localStorage.getItem(THEME_KEY);
    return stored === 'dark' || stored === 'light' || stored === 'system'
      ? stored
      : 'system';
  });
  const [theme, setTheme] = useState<'dark' | 'light'>(() =>
    resolveTheme(
      (localStorage.getItem(THEME_KEY) as ThemePref | null) ?? 'system',
    ),
  );

  useEffect(() => localStorage.setItem(VIEW_KEY, view), [view]);

  useEffect(() => {
    localStorage.setItem(THEME_KEY, themePref);
    const apply = () => {
      const resolved = resolveTheme(themePref);
      document.documentElement.dataset.theme = resolved;
      setTheme(resolved);
    };
    apply();
    if (themePref !== 'system') return;
    const mql = window.matchMedia('(prefers-color-scheme: light)');
    mql.addEventListener('change', apply);
    return () => mql.removeEventListener('change', apply);
  }, [themePref]);

  // ⌘K / Ctrl-K → recall; Esc → close overlays.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setRecallSeed('');
        setRecallOpen((v) => !v);
      } else if (e.key === 'Escape') {
        setRecallOpen(false);
        setSettingsOpen(false);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // Live updates: one WebSocket; indexing events invalidate data queries
  // (debounced) so the browser/graph refresh seconds after a session lands.
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    let ws: WebSocket | null = null;
    let closed = false;
    let retry = 1_000;
    const connect = () => {
      ws = new WebSocket(wsUrl());
      ws.onopen = () => {
        retry = 1_000;
      };
      ws.onmessage = (ev) => {
        let type = '';
        try {
          type = (JSON.parse(ev.data as string) as { type?: string }).type ?? '';
        } catch {
          return;
        }
        if (
          type === 'session_indexed' ||
          type === 'daemon_ready' ||
          type === 'scan_progress'
        ) {
          if (debounceRef.current) clearTimeout(debounceRef.current);
          debounceRef.current = setTimeout(() => {
            qc.invalidateQueries({ queryKey: ['graph'] });
            qc.invalidateQueries({ queryKey: ['projects'] });
            qc.invalidateQueries({ queryKey: ['sessions'] });
            qc.invalidateQueries({ queryKey: ['health'] });
          }, 1_200);
        }
      };
      ws.onclose = () => {
        if (closed) return;
        setTimeout(connect, retry);
        retry = Math.min(retry * 2, 15_000);
      };
    };
    connect();
    return () => {
      closed = true;
      ws?.close();
    };
  }, [qc]);

  const totalSessions = useMemo(
    () => projects.data?.reduce((acc, p) => acc + p.sessions, 0) ?? 0,
    [projects.data],
  );

  const searchContent = (q: string) => {
    setRecallSeed(q.trim());
    setRecallOpen(true);
  };

  return (
    <div className="h-full w-full flex flex-col bg-ink-950">
      <header className="px-5 py-2.5 flex items-center gap-3 shrink-0 border-b border-ink-700/60 bg-ink-900/70 backdrop-blur-sm">
        <div className="flex items-center gap-2.5">
          <span className="grid place-items-center w-8 h-8 rounded-lg bg-accent/15 border border-accent/30">
            <Waypoints className="w-4.5 h-4.5 text-accent" strokeWidth={2.2} />
          </span>
          <div className="flex flex-col leading-tight">
            <span className="font-semibold text-ink-50 text-[15px] tracking-tight">
              context-keeper
            </span>
            <span className="text-[10.5px] text-ink-300 -mt-0.5">
              your Claude Code memory
            </span>
          </div>
        </div>

        <ViewToggle view={view} onChange={setView} />

        <div className="ml-auto flex items-center gap-2">
          {health.data?.indexing && (
            <span className="flex items-center gap-1.5 text-[11px] font-mono px-2.5 py-1 rounded-full bg-accent/12 border border-accent/35 text-accent">
              <Loader2 className="w-3 h-3 animate-spin" />
              indexing · {health.data.scan_progress}
            </span>
          )}
          <button
            onClick={() => {
              setRecallSeed('');
              setRecallOpen(true);
            }}
            title="Search the content of your chats (⌘K)"
            className="flex items-center gap-2 text-[12px] px-3 py-1.5 rounded-lg bg-ink-800/60 border border-ink-700/70 text-ink-200 hover:text-ink-50 hover:border-accent/50 transition-colors"
          >
            <Search className="w-3.5 h-3.5" />
            <span>recall</span>
            <kbd className="text-[9.5px] font-mono px-1 py-0.5 rounded border border-ink-700/80 bg-ink-900/60 text-ink-300">
              ⌘K
            </kbd>
          </button>
          <ThemeButton pref={themePref} onChange={setThemePref} />
          <button
            onClick={() => setSettingsOpen(true)}
            title="Settings"
            aria-label="settings"
            className="grid place-items-center w-8 h-8 rounded-lg bg-ink-800/60 border border-ink-700/70 text-ink-300 hover:text-ink-50 hover:border-accent/50 transition-colors"
          >
            <SettingsIcon className="w-3.5 h-3.5" />
          </button>
          <div className="hidden md:flex items-center gap-1.5 text-[11px] text-ink-300 font-mono pl-1">
            <span>{projects.data?.length ?? 0} projects</span>
            <span className="opacity-40">·</span>
            <span>{totalSessions} sessions</span>
          </div>
        </div>
      </header>
      <main className="flex-1 relative" key={theme}>
        {view === 'browser' ? (
          <Browser
            projects={projects.data ?? []}
            selectedProject={project}
            onSelectProject={setProject}
            onSearchContent={searchContent}
          />
        ) : (
          <KnowledgeGraph project={null} />
        )}
        <RecallPanel
          open={recallOpen}
          project={project}
          initialQuery={recallSeed}
          onClose={() => setRecallOpen(false)}
        />
        <SettingsPanel
          open={settingsOpen}
          onClose={() => setSettingsOpen(false)}
        />
      </main>
    </div>
  );
}

function ThemeButton({
  pref,
  onChange,
}: {
  pref: ThemePref;
  onChange: (p: ThemePref) => void;
}) {
  const next: Record<ThemePref, ThemePref> = {
    system: 'dark',
    dark: 'light',
    light: 'system',
  };
  const icon =
    pref === 'dark' ? (
      <Moon className="w-3.5 h-3.5" />
    ) : pref === 'light' ? (
      <Sun className="w-3.5 h-3.5" />
    ) : (
      <Monitor className="w-3.5 h-3.5" />
    );
  return (
    <button
      onClick={() => onChange(next[pref])}
      title={`Theme: ${pref} (click to change)`}
      aria-label={`theme: ${pref}`}
      className="grid place-items-center w-8 h-8 rounded-lg bg-ink-800/60 border border-ink-700/70 text-ink-300 hover:text-ink-50 hover:border-accent/50 transition-colors"
    >
      {icon}
    </button>
  );
}

function ViewToggle({
  view,
  onChange,
}: {
  view: View;
  onChange: (v: View) => void;
}) {
  return (
    <div className="ml-1 flex items-center bg-ink-800/50 border border-ink-700/60 rounded-lg p-0.5">
      <ToggleButton
        active={view === 'browser'}
        onClick={() => onChange('browser')}
        label="Browse"
        icon={<List className="w-3.5 h-3.5" />}
      />
      <ToggleButton
        active={view === 'graph'}
        onClick={() => onChange('graph')}
        label="Graph"
        icon={<Share2 className="w-3.5 h-3.5" />}
      />
    </div>
  );
}

function ToggleButton({
  active,
  onClick,
  label,
  icon,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
  icon: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={`px-3 py-1 rounded-[7px] text-[12px] font-medium flex items-center gap-1.5 transition-colors ${
        active
          ? 'bg-accent text-accent-contrast'
          : 'text-ink-300 hover:text-ink-100'
      }`}
    >
      {icon}
      <span>{label}</span>
    </button>
  );
}
