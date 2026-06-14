// Recall panel: run the same semantic search Claude uses (`POST /v1/recall`)
// from the UI. Results show provenance (project, session, score); clicking
// one opens the full session in SessionPanel.

import { useEffect, useRef, useState } from 'react';
import { useMutation } from '@tanstack/react-query';
import { AnimatePresence, motion } from 'framer-motion';
import { Search, X, Clock, FolderTree, Loader2 } from 'lucide-react';
import { apiExtra, type RecallItem } from '../api';
import { prettyProject, pastelBg, pastelBorder, pastelText } from '../visual';
import SessionPanel from './SessionPanel';

interface Props {
  open: boolean;
  project: string | null;
  initialQuery?: string;
  onClose: () => void;
}

export default function RecallPanel({
  open,
  project,
  initialQuery,
  onClose,
}: Props) {
  const [query, setQuery] = useState('');
  const [openSession, setOpenSession] = useState<string | null>(null);
  const recall = useMutation({
    mutationFn: (q: string) => apiExtra.recall(q, project),
  });

  const submit = () => {
    const q = query.trim();
    if (q.length > 0) recall.mutate(q);
  };

  // When opened with a seed query (from the browser's "Search content"),
  // pre-fill and run it once. Re-runs whenever a new seed arrives.
  const lastSeed = useRef<string | null>(null);
  useEffect(() => {
    if (!open) {
      lastSeed.current = null;
      return;
    }
    const seed = (initialQuery ?? '').trim();
    if (seed && seed !== lastSeed.current) {
      lastSeed.current = seed;
      setQuery(seed);
      recall.mutate(seed);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, initialQuery]);

  return (
    <AnimatePresence>
      {open && (
        <motion.aside
          initial={{ x: 560, opacity: 0 }}
          animate={{ x: 0, opacity: 1 }}
          exit={{ x: 560, opacity: 0 }}
          transition={{ type: 'spring', stiffness: 230, damping: 28 }}
          className="absolute right-0 top-0 bottom-0 w-[540px] z-20 flex flex-col bg-ink-900/95 backdrop-blur-md border-l border-ink-700/60 shadow-2xl"
        >
          <div className="px-4 py-3 border-b border-ink-700/60 flex items-center gap-2 bg-gradient-to-br from-ink-800/80 to-ink-900/80">
            <Search className="w-4 h-4 text-accent" />
            <input
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && submit()}
              placeholder={
                project
                  ? `Search your memory in ${prettyProject(project)}…`
                  : 'Search your memory across every project…'
              }
              className="flex-1 bg-transparent outline-none text-[13.5px] text-ink-50 placeholder:text-ink-300/60"
            />
            <button
              onClick={submit}
              disabled={recall.isPending || query.trim().length === 0}
              className="text-[12px] font-semibold px-3.5 py-1.5 rounded-lg bg-accent text-accent-contrast disabled:opacity-40 hover:brightness-110 transition"
            >
              {recall.isPending ? (
                <Loader2 className="w-3.5 h-3.5 animate-spin" />
              ) : (
                'recall'
              )}
            </button>
            <button
              onClick={onClose}
              className="grid place-items-center w-8 h-8 rounded-xl bg-ink-800/80 border border-ink-700/60 text-ink-300 hover:text-ink-50 transition-colors"
              aria-label="close recall"
            >
              <X className="w-4 h-4" />
            </button>
          </div>

          <div className="flex-1 min-h-0 overflow-y-auto ck-scroll">
            {recall.isError && (
              <div className="px-4 py-3 text-[12px] text-red-300">
                {(recall.error as Error).message}
              </div>
            )}
            {recall.data && recall.data.items.length === 0 && (
              <div className="px-4 py-8 text-center text-[12.5px] text-ink-300">
                Nothing above the relevance floor for that query.
              </div>
            )}
            {recall.data && recall.data.items.length > 0 && (
              <>
                <div className="px-4 py-2 text-[10.5px] text-ink-300/80 font-mono border-b border-ink-800/60">
                  {recall.data.items.length} chunks ·{' '}
                  {recall.data.total_tokens.toLocaleString()} tokens ·{' '}
                  {recall.data.elapsed_ms}ms
                  {recall.data.truncated ? ' · budget-truncated' : ''}
                </div>
                {recall.data.items.map((it) => (
                  <ResultRow
                    key={it.chunk_id}
                    item={it}
                    onOpen={() => setOpenSession(it.session_id)}
                  />
                ))}
              </>
            )}
            {!recall.data && !recall.isError && !recall.isPending && (
              <div className="px-4 py-8 text-center text-[12.5px] text-ink-300/80 leading-relaxed">
                This is the same <span className="font-mono">recall</span> Claude
                calls over MCP — ranked, diversity-re-ranked, token-budgeted.
                <br />
                Try “what did we decide about auth?”
              </div>
            )}
          </div>

          <SessionPanel
            sessionId={openSession}
            onClose={() => setOpenSession(null)}
          />
        </motion.aside>
      )}
    </AnimatePresence>
  );
}

function ResultRow({ item, onOpen }: { item: RecallItem; onOpen: () => void }) {
  const pct = Math.round(item.score * 100);
  return (
    <button
      onClick={onOpen}
      className="w-full text-left px-4 py-3 border-b border-ink-800/50 hover:bg-ink-800/40 transition-colors group"
    >
      <div className="flex items-center gap-2 mb-1">
        <span
          className="text-[10px] px-2 py-0.5 rounded-full font-semibold"
          style={{
            background: pastelBg(item.project, 0.18),
            border: `1px solid ${pastelBorder(item.project, 0.4)}`,
            color: pastelText(item.project),
          }}
        >
          <FolderTree className="w-2.5 h-2.5 inline mr-1 -mt-0.5" />
          {prettyProject(item.project)}
        </span>
        <span className="text-[11px] text-ink-200 truncate flex-1">
          {item.session_title ?? item.session_id.slice(0, 12)}
        </span>
        <span className="text-[10px] font-mono text-accent shrink-0">
          {pct}%
        </span>
      </div>
      <div className="h-1 rounded-full bg-ink-800/80 mb-1.5 overflow-hidden">
        <div
          className="h-full rounded-full bg-accent/80"
          style={{ width: `${pct}%` }}
        />
      </div>
      <p className="text-[12px] leading-snug text-ink-100/90 line-clamp-3 whitespace-pre-wrap">
        {item.text}
      </p>
      {/* C4 provenance: a calm secondary line — why this chunk was recalled.
          Theme tokens, italic, low-emphasis; not a loud chip. */}
      {item.why && (
        <p className="mt-1 text-[10.5px] italic text-ink-300/70 truncate">
          {item.why}
        </p>
      )}
      <div className="mt-1 flex items-center gap-2 text-[10px] text-ink-300/70 font-mono">
        <Clock className="w-2.5 h-2.5" />
        {item.started_at.slice(0, 16).replace('T', ' ')}
        <span>· {item.token_count}t</span>
        <span className="ml-auto opacity-0 group-hover:opacity-100 transition-opacity text-accent">
          open session →
        </span>
      </div>
    </button>
  );
}
