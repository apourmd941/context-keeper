// Tiny helpers for the playful visual layer: deterministic per-string
// pastel hue, emoji picker keyed off label keywords, and pretty-printers.

const EMOJI_KEYWORDS: Array<[RegExp, string]> = [
  [/chunk|token|split/i, '🪓'],
  [/embed|vector|cosine/i, '🧬'],
  [/daemon|service|server|axum/i, '⚙️'],
  [/mcp|tool|protocol|rpc/i, '🔌'],
  [/cluster|topic|graph|dbscan/i, '🕸️'],
  [/cache|store|index|sqlite/i, '🗄️'],
  [/recall|search|query/i, '🔎'],
  [/summary|summariz|haiku|llm/i, '✨'],
  [/web|ui|react|frontend|css/i, '🎨'],
  [/api|http|endpoint|route/i, '🛰️'],
  [/test|fixture|spec/i, '🧪'],
  [/transcript|jsonl|parse|record/i, '📜'],
  [/plan|milestone|roadmap/i, '🗺️'],
  [/bug|fix|error|panic/i, '🐛'],
  [/perf|fast|slow|latency|benchmark/i, '⚡'],
  [/auth|key|cred|secret/i, '🔐'],
  [/file|path|fs|write|read/i, '📁'],
  [/git|commit|branch|diff/i, '🌿'],
  [/hook|event|subscribe/i, '🪝'],
  [/sidechain|subagent|child/i, '🧒'],
  [/setup|install|build|cargo|pnpm/i, '🛠️'],
  [/decision|design|architect/i, '🧭'],
  [/doc|readme|instructions/i, '📘'],
];

export function emojiFor(label: string): string {
  for (const [re, emoji] of EMOJI_KEYWORDS) {
    if (re.test(label)) return emoji;
  }
  // Hash fallback into a small palette so the same label always lands on
  // the same emoji.
  const palette = ['🌱', '🌸', '🌊', '🍋', '🪐', '🎈', '🍄', '🪴', '🦋', '🌟'];
  let h = 0;
  for (let i = 0; i < label.length; i++) h = (h * 31 + label.charCodeAt(i)) | 0;
  return palette[Math.abs(h) % palette.length];
}

// Deterministic pastel hue (0-360) from any string. Same project name,
// same color, every reload.
export function hueFor(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 131 + s.charCodeAt(i)) | 0;
  return Math.abs(h) % 360;
}

function isLight(): boolean {
  return (
    typeof document !== 'undefined' &&
    document.documentElement.dataset.theme === 'light'
  );
}

export function pastelBg(s: string, alpha = 0.18): string {
  return isLight()
    ? `hsla(${hueFor(s)}, 65%, 45%, ${alpha * 0.8})`
    : `hsla(${hueFor(s)}, 75%, 65%, ${alpha})`;
}
export function pastelBorder(s: string, alpha = 0.55): string {
  return isLight()
    ? `hsla(${hueFor(s)}, 55%, 40%, ${alpha * 0.8})`
    : `hsla(${hueFor(s)}, 70%, 70%, ${alpha})`;
}
export function pastelText(s: string): string {
  return isLight()
    ? `hsl(${hueFor(s)}, 70%, 30%)`
    : `hsl(${hueFor(s)}, 80%, 85%)`;
}
/** Solid node fill for canvas-drawn graphs (needs real colors, not CSS vars). */
export function pastelNode(s: string): string {
  return isLight()
    ? `hsl(${hueFor(s)}, 60%, 52%)`
    : `hsl(${hueFor(s)}, 70%, 66%)`;
}

// "/Users/me/Development" reads better than "-Users-me-Development".
export function prettyProject(id: string): string {
  if (id.startsWith('-')) {
    return id.slice(1).split('-').slice(-2).join('/');
  }
  return id;
}

// First non-empty line, capped, for use as a tooltip preview.
export function firstLine(s: string | undefined, max = 120): string {
  if (!s) return '';
  const line = s.split('\n').find((l) => l.trim().length > 0) ?? '';
  return line.length > max ? line.slice(0, max - 1) + '…' : line;
}
