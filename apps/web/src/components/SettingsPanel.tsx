// Settings: edits ~/.context-keeper/config.toml via GET/PUT /v1/settings.
// Server validates; errors surface inline. These settings govern the
// auto-recall hook's behavior (no env vars needed) and auto-promotion.

import { useEffect, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { AnimatePresence, motion } from 'framer-motion';
import { Settings as SettingsIcon, X, Check, Loader2 } from 'lucide-react';
import { apiExtra, type Settings } from '../api';

interface Props {
  open: boolean;
  onClose: () => void;
}

export default function SettingsPanel({ open, onClose }: Props) {
  const qc = useQueryClient();
  const current = useQuery({
    queryKey: ['settings'],
    queryFn: apiExtra.getSettings,
    enabled: open,
  });
  const [draft, setDraft] = useState<Settings | null>(null);
  const [savedPath, setSavedPath] = useState<string | null>(null);

  useEffect(() => {
    if (current.data && open) setDraft(structuredClone(current.data));
    if (!open) setSavedPath(null);
  }, [current.data, open]);

  const save = useMutation({
    mutationFn: (s: Settings) => apiExtra.putSettings(s),
    onSuccess: (r) => {
      setSavedPath(r.path);
      qc.invalidateQueries({ queryKey: ['settings'] });
    },
  });

  return (
    <AnimatePresence>
      {open && (
        <>
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            onClick={onClose}
            className="absolute inset-0 z-20 bg-ink-950/60 backdrop-blur-[2px]"
          />
          <motion.section
            initial={{ y: 24, opacity: 0, scale: 0.98 }}
            animate={{ y: 0, opacity: 1, scale: 1 }}
            exit={{ y: 24, opacity: 0, scale: 0.98 }}
            transition={{ type: 'spring', stiffness: 260, damping: 26 }}
            className="absolute z-30 left-1/2 top-16 -translate-x-1/2 w-[480px] rounded-2xl border border-ink-700/60 bg-ink-900/95 backdrop-blur-md shadow-2xl"
          >
            <div className="px-4 py-3 border-b border-ink-700/60 flex items-center gap-2">
              <SettingsIcon className="w-4 h-4 text-accent" />
              <span className="font-bold text-ink-50 text-[14px]">Settings</span>
              <span className="text-[10px] text-ink-300/70 font-mono ml-1">
                config.toml
              </span>
              <button
                onClick={onClose}
                className="ml-auto grid place-items-center w-8 h-8 rounded-xl bg-ink-800/80 border border-ink-700/60 text-ink-300 hover:text-ink-50 transition-colors"
                aria-label="close settings"
              >
                <X className="w-4 h-4" />
              </button>
            </div>

            {!draft ? (
              <div className="px-4 py-6 text-[12px] text-ink-300 flex items-center gap-2">
                <Loader2 className="w-4 h-4 animate-spin" /> loading…
              </div>
            ) : (
              <div className="px-4 py-4 space-y-4">
                <label className="flex items-start gap-3 cursor-pointer">
                  <input
                    type="checkbox"
                    checked={draft.auto_promote}
                    onChange={(e) =>
                      setDraft({ ...draft, auto_promote: e.target.checked })
                    }
                    className="mt-0.5 accent-[var(--accent)]"
                  />
                  <span>
                    <span className="block text-[12.5px] text-ink-50 font-semibold">
                      Auto-promote hot chunks
                    </span>
                    <span className="block text-[11px] text-ink-300 leading-snug">
                      Frequently recalled chunks get summarized into the
                      project's CLAUDE.md so future sessions start warm.
                    </span>
                  </span>
                </label>

                <div className="border-t border-ink-800/70 pt-3">
                  <div className="text-[10px] uppercase tracking-widest font-bold text-ink-300 mb-2">
                    auto-recall hook
                  </div>
                  <div className="grid grid-cols-2 gap-3">
                    <Field
                      label="relevance floor"
                      hint="0–1; hits below are dropped"
                      value={draft.hook.score_threshold}
                      step={0.05}
                      onChange={(v) =>
                        setDraft({
                          ...draft,
                          hook: { ...draft.hook, score_threshold: v },
                        })
                      }
                    />
                    <Field
                      label="max chunks"
                      hint="per prompt"
                      value={draft.hook.limit}
                      step={1}
                      onChange={(v) =>
                        setDraft({
                          ...draft,
                          hook: { ...draft.hook, limit: Math.round(v) },
                        })
                      }
                    />
                    <Field
                      label="token budget"
                      hint="across injected chunks"
                      value={draft.hook.token_budget}
                      step={100}
                      onChange={(v) =>
                        setDraft({
                          ...draft,
                          hook: { ...draft.hook, token_budget: Math.round(v) },
                        })
                      }
                    />
                    <Field
                      label="min words"
                      hint="skip shorter prompts"
                      value={draft.hook.min_words}
                      step={1}
                      onChange={(v) =>
                        setDraft({
                          ...draft,
                          hook: { ...draft.hook, min_words: Math.round(v) },
                        })
                      }
                    />
                  </div>
                  <div className="mt-3">
                    <div className="text-[11px] text-ink-300 mb-1">scope</div>
                    <div className="flex gap-2">
                      {(['project', 'global'] as const).map((s) => (
                        <button
                          key={s}
                          onClick={() =>
                            setDraft({
                              ...draft,
                              hook: { ...draft.hook, scope: s },
                            })
                          }
                          className={`px-3 py-1 rounded-xl text-[11px] font-bold uppercase tracking-widest border transition-colors ${
                            draft.hook.scope === s
                              ? 'bg-accent text-accent-contrast border-accent'
                              : 'bg-ink-800/60 text-ink-300 border-ink-700/60 hover:text-ink-100'
                          }`}
                        >
                          {s}
                        </button>
                      ))}
                    </div>
                  </div>
                </div>

                {save.isError && (
                  <div className="text-[11.5px] text-red-300">
                    {(save.error as Error).message}
                  </div>
                )}
                {savedPath && !save.isError && (
                  <div className="text-[11.5px] text-emerald-300 flex items-center gap-1.5">
                    <Check className="w-3.5 h-3.5" /> saved to{' '}
                    <span className="font-mono">{savedPath}</span>
                  </div>
                )}

                <div className="flex justify-end gap-2 pt-1">
                  <button
                    onClick={onClose}
                    className="px-3 py-1.5 rounded-xl text-[12px] text-ink-300 hover:text-ink-100 transition-colors"
                  >
                    close
                  </button>
                  <button
                    onClick={() => draft && save.mutate(draft)}
                    disabled={save.isPending}
                    className="px-4 py-1.5 rounded-xl text-[12px] font-bold bg-accent text-accent-contrast disabled:opacity-50 hover:brightness-110 transition"
                  >
                    {save.isPending ? 'saving…' : 'save'}
                  </button>
                </div>
              </div>
            )}
          </motion.section>
        </>
      )}
    </AnimatePresence>
  );
}

function Field({
  label,
  hint,
  value,
  step,
  onChange,
}: {
  label: string;
  hint: string;
  value: number;
  step: number;
  onChange: (v: number) => void;
}) {
  return (
    <label className="block">
      <span className="block text-[11px] text-ink-300">{label}</span>
      <input
        type="number"
        value={value}
        step={step}
        onChange={(e) => onChange(Number(e.target.value))}
        className="mt-0.5 w-full bg-ink-800/70 border border-ink-700/60 rounded-xl px-2.5 py-1.5 text-[12.5px] text-ink-50 outline-none focus:border-accent/60"
      />
      <span className="block text-[9.5px] text-ink-300/60 mt-0.5">{hint}</span>
    </label>
  );
}
