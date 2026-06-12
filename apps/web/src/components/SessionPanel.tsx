// Side panel: session metadata, AI summary, virtualized transcript.
// framer-motion handles the slide-in / fade-out, react-virtuoso keeps
// long transcripts smooth, react-syntax-highlighter colorizes code blocks.

import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { AnimatePresence, motion } from 'framer-motion';
import { Virtuoso } from 'react-virtuoso';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { oneDark } from 'react-syntax-highlighter/dist/esm/styles/prism';
import {
  X,
  User,
  Bot,
  MessageSquare,
  Wrench,
  Clock,
  Hash,
  FolderTree,
  Sparkles,
  ListChecks,
  CheckCircle2,
  FileCode,
} from 'lucide-react';
import { api, type SessionRecord, type TranscriptEntry } from '../api';
import { prettyProject } from '../visual';

interface Props {
  sessionId: string | null;
  onClose: () => void;
}

export default function SessionPanel({ sessionId, onClose }: Props) {
  const session = useQuery({
    queryKey: ['session', sessionId],
    queryFn: () => api.session(sessionId!),
    enabled: !!sessionId,
  });
  const transcript = useQuery({
    queryKey: ['transcript', sessionId],
    queryFn: () => api.transcript(sessionId!),
    enabled: !!sessionId,
  });

  return (
    <AnimatePresence>
      {sessionId && (
        <motion.aside
          key={sessionId}
          initial={{ x: 540, opacity: 0 }}
          animate={{ x: 0, opacity: 1 }}
          exit={{ x: 540, opacity: 0 }}
          transition={{ type: 'spring', stiffness: 220, damping: 28 }}
          className="absolute right-0 top-0 bottom-0 w-[520px] z-10 flex flex-col bg-ink-900/95 backdrop-blur-md border-l border-ink-700/60 shadow-2xl"
        >
          <PanelHeader
            session={session.data}
            sessionId={sessionId}
            onClose={onClose}
          />
          <PanelMeta session={session.data} sessionId={sessionId} />
          {session.data?.summary && <PanelSummary session={session.data} />}
          <div className="flex-1 min-h-0">
            {transcript.isLoading && (
              <div className="px-4 py-3 text-xs text-ink-300 flex items-center gap-2">
                <span className="w-2 h-2 rounded-full bg-project animate-pulse" />
                loading transcript…
              </div>
            )}
            {transcript.data && (
              <Virtuoso
                className="ck-scroll h-full"
                data={transcript.data}
                itemContent={(_, c) => <TurnRow c={c} />}
              />
            )}
          </div>
        </motion.aside>
      )}
    </AnimatePresence>
  );
}

function PanelHeader({
  session,
  sessionId,
  onClose,
}: {
  session: SessionRecord | undefined;
  sessionId: string;
  onClose: () => void;
}) {
  const title =
    session?.ai_title ?? session?.first_prompt?.slice(0, 80) ?? sessionId;
  return (
    <div className="px-4 py-3 border-b border-ink-700/60 flex items-start gap-3 bg-gradient-to-br from-ink-800/80 to-ink-900/80">
      <span
        className={`grid place-items-center w-9 h-9 mt-0.5 rounded-lg border ${
          session?.is_sidechain
            ? 'bg-project/15 border-project/40 text-project'
            : 'bg-accent/15 border-accent/40 text-accent'
        }`}
      >
        {session?.is_sidechain ? (
          <Bot className="w-4.5 h-4.5" />
        ) : (
          <MessageSquare className="w-4.5 h-4.5" />
        )}
      </span>
      <div className="min-w-0 flex-1">
        <div className="text-[10px] uppercase tracking-widest text-ink-300 font-bold">
          {session?.is_sidechain ? 'subagent session' : 'session'}
        </div>
        <div className="font-bold text-ink-50 truncate text-[15px]">
          {title}
        </div>
      </div>
      <button
        onClick={onClose}
        className="grid place-items-center w-8 h-8 rounded-xl bg-ink-800/80 border border-ink-700/60 text-ink-300 hover:text-ink-50 hover:border-project/60 transition-colors"
        aria-label="close"
      >
        <X className="w-4 h-4" />
      </button>
    </div>
  );
}

function PanelMeta({
  session,
  sessionId,
}: {
  session: SessionRecord | undefined;
  sessionId: string;
}) {
  if (!session) {
    return (
      <div className="px-4 py-3 text-[11px] text-ink-300 font-mono break-all">
        {sessionId}
      </div>
    );
  }
  const fmt = (s: string) => s.slice(0, 16).replace('T', ' ');
  return (
    <div className="px-4 py-3 border-b border-ink-700/40 grid grid-cols-2 gap-2 text-[11px]">
      <MetaPill icon={<FolderTree className="w-3 h-3" />} label="project">
        {prettyProject(session.project_id)}
      </MetaPill>
      <MetaPill icon={<Hash className="w-3 h-3" />} label="messages">
        {session.message_count} · {session.chunk_ids.length} chunks
      </MetaPill>
      <MetaPill icon={<Clock className="w-3 h-3" />} label="started">
        {fmt(session.started_at)}
      </MetaPill>
      <MetaPill icon={<Clock className="w-3 h-3" />} label="ended">
        {fmt(session.ended_at)}
      </MetaPill>
    </div>
  );
}

function MetaPill({
  icon,
  label,
  children,
}: {
  icon: React.ReactNode;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="rounded-xl bg-ink-800/60 border border-ink-700/50 px-2.5 py-1.5">
      <div className="flex items-center gap-1 text-ink-300 text-[9px] uppercase tracking-widest font-bold">
        {icon}
        <span>{label}</span>
      </div>
      <div className="text-ink-100 text-[11.5px] truncate font-medium">
        {children}
      </div>
    </div>
  );
}

function PanelSummary({ session }: { session: SessionRecord }) {
  const sum = session.summary!;
  return (
    <div className="px-4 py-3 border-b border-ink-700/40 bg-gradient-to-br from-topic/[0.10] to-project/[0.06]">
      <div className="flex items-center gap-1.5 text-[10px] uppercase tracking-widest font-bold text-topic-soft mb-2">
        <Sparkles className="w-3 h-3" />
        ai summary
      </div>
      <p className="text-[12.5px] text-ink-100 leading-snug mb-2 italic">
        {sum.text}
      </p>
      {sum.bullets.length > 0 && (
        <SummaryList
          icon={<ListChecks className="w-3 h-3" />}
          label="bullets"
          items={sum.bullets}
          tone="bullets"
        />
      )}
      {sum.decisions.length > 0 && (
        <SummaryList
          icon={<CheckCircle2 className="w-3 h-3" />}
          label="decisions"
          items={sum.decisions}
          tone="decisions"
        />
      )}
      {sum.artifacts.length > 0 && (
        <SummaryList
          icon={<FileCode className="w-3 h-3" />}
          label="artifacts"
          items={sum.artifacts}
          tone="artifacts"
        />
      )}
    </div>
  );
}

function SummaryList({
  icon,
  label,
  items,
  tone,
}: {
  icon: React.ReactNode;
  label: string;
  items: string[];
  tone: 'bullets' | 'decisions' | 'artifacts';
}) {
  const toneClass =
    tone === 'decisions'
      ? 'text-session-soft'
      : tone === 'artifacts'
        ? 'text-project-soft'
        : 'text-topic-soft';
  return (
    <div className="mt-2">
      <div
        className={`flex items-center gap-1 text-[9px] uppercase tracking-widest font-bold ${toneClass}`}
      >
        {icon}
        <span>{label}</span>
      </div>
      <ul className="mt-1 space-y-0.5 text-[11.5px] text-ink-200">
        {items.map((b, i) => (
          <li key={i} className="flex gap-1.5 leading-snug">
            <span className="opacity-50">•</span>
            <span>{b}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function TurnRow({ c }: { c: TranscriptEntry }) {
  const role = c.role;
  const meta = roleMeta(role, !!c.tool_name);
  const blocks = useMemo(() => splitCodeBlocks(c.text), [c.text]);
  return (
    <div className="px-4 py-2 border-b border-ink-800/40">
      <div className="flex items-center gap-2 mb-1">
        <span
          className={`grid place-items-center w-5 h-5 rounded-full ${meta.bg}`}
        >
          {meta.icon}
        </span>
        <span
          className={`text-[10px] uppercase tracking-widest font-bold ${meta.text}`}
        >
          {meta.label}
        </span>
        {c.tool_name && (
          <span className="text-[10px] font-mono px-1.5 py-0.5 rounded-md bg-project/20 text-project-soft border border-project/30">
            {c.tool_name}
          </span>
        )}
        <span className="ml-auto text-[10px] text-ink-300/70 font-mono">
          {c.token_count}t
        </span>
      </div>
      <div className="ml-7 text-[12px] leading-snug text-ink-100 space-y-1">
        {blocks.map((b, i) =>
          b.kind === 'code' ? (
            <CodeBlock key={i} lang={b.lang} code={b.text} />
          ) : (
            <p key={i} className="whitespace-pre-wrap font-sans break-words">
              {b.text}
            </p>
          ),
        )}
      </div>
    </div>
  );
}

function roleMeta(role: string, hasTool: boolean) {
  if (hasTool) {
    return {
      label: 'tool',
      icon: <Wrench className="w-3 h-3 text-ink-950" strokeWidth={2.6} />,
      bg: 'bg-project',
      text: 'text-project-soft',
    };
  }
  if (role === 'user') {
    return {
      label: 'you',
      icon: <User className="w-3 h-3 text-ink-950" strokeWidth={2.6} />,
      bg: 'bg-session',
      text: 'text-session-soft',
    };
  }
  if (role === 'assistant') {
    return {
      label: 'claude',
      icon: <Bot className="w-3 h-3 text-ink-950" strokeWidth={2.6} />,
      bg: 'bg-topic',
      text: 'text-topic-soft',
    };
  }
  return {
    label: role,
    icon: <Hash className="w-3 h-3 text-ink-950" strokeWidth={2.6} />,
    bg: 'bg-ink-300',
    text: 'text-ink-200',
  };
}

function CodeBlock({ lang, code }: { lang: string; code: string }) {
  return (
    <div className="rounded-xl overflow-hidden border border-ink-700/50 my-1.5">
      <div className="flex items-center justify-between px-2.5 py-1 bg-ink-900/80 border-b border-ink-700/50">
        <span className="text-[9px] uppercase tracking-widest font-bold text-ink-300">
          {lang || 'code'}
        </span>
      </div>
      <SyntaxHighlighter
        language={lang || 'text'}
        style={oneDark}
        customStyle={{
          margin: 0,
          padding: '8px 10px',
          fontSize: '11px',
          background: '#15100a',
          fontFamily: '"JetBrains Mono", monospace',
        }}
        wrapLongLines
      >
        {code}
      </SyntaxHighlighter>
    </div>
  );
}

// Split a chunk text into prose / code segments by triple-backtick fences.
// We render each segment with its own component. Cheap regex parser is
// fine — the chunker already kept chunks small.
type Segment = { kind: 'text' | 'code'; text: string; lang: string };
function splitCodeBlocks(text: string): Segment[] {
  const out: Segment[] = [];
  const re = /```(\w*)\n([\s\S]*?)```/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    if (m.index > last) {
      out.push({ kind: 'text', text: text.slice(last, m.index), lang: '' });
    }
    out.push({ kind: 'code', text: m[2], lang: m[1] || '' });
    last = m.index + m[0].length;
  }
  if (last < text.length) {
    out.push({ kind: 'text', text: text.slice(last), lang: '' });
  }
  return out.filter((s) => s.text.trim().length > 0);
}
