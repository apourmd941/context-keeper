// Obsidian-style 2D force-directed knowledge graph.
//
// Three node types — project / topic / session — sized and colored by
// kind. Links: project→topic, topic→session, plus topic-similarity
// edges. Clicking a node enters focus mode: the node + its direct
// neighbors stay vivid, everything else fades to ~10% alpha.
//
// Right-side panel exposes search, type filters, and live force tuning
// (charge, link strength, link distance, center). Hover shows a
// tooltip; single-click on a session opens the existing SessionPanel.

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import { useQuery } from '@tanstack/react-query';
import ForceGraph2D, {
  type ForceGraphMethods,
  type LinkObject,
  type NodeObject,
} from 'react-force-graph-2d';
import { motion, AnimatePresence } from 'framer-motion';
import {
  Search,
  Sliders,
  FolderTree,
  Network as NetworkIcon,
  MessageSquare,
  Filter,
  RotateCcw,
  Sparkles,
  CalendarRange,
  ChevronDown,
} from 'lucide-react';
import { api, type GraphNode, type GraphEdge } from '../api';
import { prettyProject } from '../visual';
import SessionPanel from './SessionPanel';

interface Props {
  project: string | null;
}

type NodeKind = 'project' | 'topic' | 'session';

interface GNode {
  id: string;
  kind: NodeKind;
  label: string;
  size: number;
  // Wire-format helpers retained for the side panel + hover tooltip.
  sessionCount?: number;
  chunkCount?: number;
  projectId?: string;
  bareSessionId?: string;
  endedAt?: string; // ISO timestamp on session nodes; used for date range filter
  cwd?: string | null; // real filesystem path on project nodes
}

interface GLink {
  source: string;
  target: string;
  kind: 'contains' | 'similarity';
}

// Obsidian-style palette on pure black. Each kind gets a vivid fill so
// the legend reads clearly even at small dot sizes.
const COLOR: Record<NodeKind, string> = {
  project: '#f59e0b', // amber-500
  topic: '#fb7185', // rose-400
  session: '#10b981', // emerald-500
};
const COLOR_HIGHLIGHT = '#a78bfa'; // violet-400 — focus purple, lifted for dark
// True blue (not cyan), high alpha so the threads read as luminous on
// pure black. Highlight is a brighter blue for the focused node.
const COLOR_LINK_DEFAULT = 'rgba(96, 165, 250, 0.85)'; // blue-400
const COLOR_LINK_HIGHLIGHT = 'rgba(147, 197, 253, 1)'; // blue-300, bright pulse
const COLOR_LINK_DIM = 'rgba(96, 165, 250, 0.10)';
// On black, labels read best as the light tint of their node hue with a
// black outline.
const LABEL_COLOR: Record<NodeKind, string> = {
  project: '#fde68a', // amber-200
  topic: '#fecdd3', // rose-200
  session: '#a7f3d0', // emerald-200
};
const LABEL_OUTLINE = 'rgba(0, 0, 0, 0.95)';
/** Canvas bg resolved from the theme at render time (force-graph paints
 *  real pixels, so it can't consume CSS vars directly). The canvas subtree
 *  remounts on theme change, so reading per-render is safe. */
function canvasBg(): string {
  if (typeof document === 'undefined') return '#0B1220';
  return document.documentElement.dataset.theme === 'light'
    ? '#EDF1F7'
    : '#06090F';
}

interface Forces {
  charge: number; // repel strength (negative number)
  linkStrength: number; // 0..1
  linkDistance: number; // px
  center: number; // 0..1
}

const DEFAULT_FORCES: Forces = {
  charge: -260,
  linkStrength: 0.45,
  linkDistance: 110,
  center: 0.04,
};

export default function KnowledgeGraph({ project: _project }: Props) {
  const graph = useQuery({
    queryKey: ['graph', null],
    queryFn: () => api.graph(null),
    refetchInterval: 60_000, // WS invalidation is the live path; this is the safety net
  });

  const [selectedSession, setSelectedSession] = useState<string | null>(null);
  const [focusId, setFocusId] = useState<string | null>(null);
  const [search, setSearch] = useState('');
  const [forces, setForces] = useState<Forces>(DEFAULT_FORCES);
  const [showKinds, setShowKinds] = useState<Record<NodeKind, boolean>>({
    project: true,
    topic: true,
    session: true,
  });
  // Left-panel filters: per-project allow-list + date range (YYYY-MM-DD).
  // `selectedProjects === null` means "no filter, show everything"; an
  // empty set means "exclude everything" — keep them distinct so the
  // user can clear all without losing the original list.
  const [selectedProjects, setSelectedProjects] = useState<Set<string> | null>(
    null,
  );
  const [dateStart, setDateStart] = useState<string>('');
  const [dateEnd, setDateEnd] = useState<string>('');
  const [hovered, setHovered] = useState<{
    label: string;
    sub: string;
    x: number;
    y: number;
  } | null>(null);

  const data = useMemo(
    () => buildGraph(graph.data?.nodes ?? [], graph.data?.edges ?? []),
    [graph.data],
  );

  // Resolve a link endpoint to its bare id whether react-force-graph has
  // already mutated source/target into node-object refs or not.
  const endpointId = (e: string | NodeObject<GNode>): string =>
    typeof e === 'string' ? e : (e.id as string);

  // O(1) node lookup by id — used by visibility + neighbor expansion.
  const nodeById = useMemo(() => {
    const m = new Map<string, GNode>();
    for (const n of data.nodes) m.set(n.id, n);
    return m;
  }, [data.nodes]);

  // Neighbor index built once over ALL links (built before visibleIds so
  // search expansion can use it).
  const neighbors = useMemo(() => {
    const map = new Map<string, Set<string>>();
    for (const l of data.links) {
      const a = endpointId(l.source as string | NodeObject<GNode>);
      const b = endpointId(l.target as string | NodeObject<GNode>);
      if (!map.has(a)) map.set(a, new Set());
      if (!map.has(b)) map.set(b, new Set());
      map.get(a)!.add(b);
      map.get(b)!.add(a);
    }
    return map;
  }, [data.links]);

  // Surface all distinct projects from the dataset for the filter list.
  const allProjects = useMemo(() => {
    const seen = new Set<string>();
    const out: { id: string; label: string }[] = [];
    for (const n of data.nodes) {
      if (n.kind === 'project' && !seen.has(n.id)) {
        seen.add(n.id);
        out.push({ id: n.id, label: n.label });
      }
    }
    // Sessions also carry a project_id — include any project that has
    // sessions but isn't represented as its own project node.
    for (const n of data.nodes) {
      if (n.kind === 'session' && n.projectId && !seen.has(n.projectId)) {
        seen.add(n.projectId);
        out.push({ id: n.projectId, label: prettyProject(n.projectId) });
      }
    }
    out.sort((a, b) => a.label.localeCompare(b.label));
    return out;
  }, [data.nodes]);

  // Visibility composes four filters: kind toggle, project allow-list,
  // date range, and search. All are evaluated up-front so the simulation
  // doesn't re-init per change.
  //
  // Two modes:
  //  - No search → cascade: sessions pass data filters first; parents
  //    (topics, projects) only stay visible if they have at least one
  //    visible descendant (so we don't strand empty nodes).
  //  - Active search → inclusion: any node whose label/id matches is
  //    visible (subject to the *non-search* filters), and its 1-hop
  //    neighbors are pulled in too so matches don't appear orphaned.
  const visibleIds = useMemo(() => {
    const q = search.trim().toLowerCase();
    const matchesQ = (n: GNode) =>
      !q || n.label.toLowerCase().includes(q) || n.id.toLowerCase().includes(q);
    const inProjects = (pid: string) =>
      selectedProjects === null || selectedProjects.has(pid);
    const inDate = (iso?: string) => {
      if (!iso) return true;
      if (!dateStart && !dateEnd) return true;
      const d = iso.slice(0, 10);
      if (dateStart && d < dateStart) return false;
      if (dateEnd && d > dateEnd) return false;
      return true;
    };
    // Filters that apply regardless of search (kind + project + date).
    const passesFilters = (n: GNode) => {
      if (!showKinds[n.kind]) return false;
      if (n.kind === 'session') {
        if (n.projectId && !inProjects(n.projectId)) return false;
        if (!inDate(n.endedAt)) return false;
      }
      if (n.kind === 'project' && !inProjects(n.id)) return false;
      return true;
    };

    // ---- search-active path ------------------------------------------
    if (q) {
      const matched = new Set<string>();
      for (const n of data.nodes) {
        if (!passesFilters(n)) continue;
        if (matchesQ(n)) matched.add(n.id);
      }
      // Expand 1 hop so matched nodes show with their immediate context.
      const visible = new Set(matched);
      for (const id of matched) {
        const ns = neighbors.get(id);
        if (!ns) continue;
        for (const nid of ns) {
          const nb = nodeById.get(nid);
          if (nb && passesFilters(nb)) visible.add(nid);
        }
      }
      return visible;
    }

    // ---- no-search cascade path -------------------------------------
    const visibleSessions = new Set<string>();
    const visibleProjectsFromSessions = new Set<string>();
    for (const n of data.nodes) {
      if (n.kind !== 'session') continue;
      if (!passesFilters(n)) continue;
      visibleSessions.add(n.id);
      if (n.projectId) visibleProjectsFromSessions.add(n.projectId);
    }

    const visible = new Set<string>(visibleSessions);
    for (const n of data.nodes) {
      if (n.kind === 'session') continue;
      if (!passesFilters(n)) continue;
      const kids = neighbors.get(n.id);
      const hasVisibleDescendant = !!(
        kids && [...kids].some((k) => visibleSessions.has(k))
      );
      const reachableViaTopics =
        n.kind === 'project' && visibleProjectsFromSessions.has(n.id);
      if (hasVisibleDescendant || reachableViaTopics) visible.add(n.id);
    }

    return visible;
  }, [
    data.nodes,
    neighbors,
    nodeById,
    search,
    showKinds,
    selectedProjects,
    dateStart,
    dateEnd,
  ]);

  const fgRef = useRef<ForceGraphMethods<NodeObject<GNode>, LinkObject<GNode, GLink>>>(
    undefined as unknown as ForceGraphMethods<
      NodeObject<GNode>,
      LinkObject<GNode, GLink>
    >,
  );

  // Push slider state into the d3-force simulation. We re-heat the
  // simulation after each change so the graph visibly re-settles.
  useEffect(() => {
    const fg = fgRef.current;
    if (!fg) return;
    const charge = fg.d3Force('charge') as unknown as
      | { strength: (v: number) => void }
      | undefined;
    charge?.strength(forces.charge);
    const link = fg.d3Force('link') as unknown as
      | { strength: (v: number) => void; distance: (v: number) => void }
      | undefined;
    link?.strength(forces.linkStrength);
    link?.distance(forces.linkDistance);
    const center = fg.d3Force('center') as unknown as
      | { strength?: (v: number) => void }
      | undefined;
    center?.strength?.(forces.center);
    fg.d3ReheatSimulation();
  }, [forces]);

  const onNodeClick = useCallback(
    (n: NodeObject<GNode>) => {
      setFocusId((cur) => (cur === n.id ? null : (n.id as string)));
      if (n.kind === 'session' && n.bareSessionId) {
        setSelectedSession(n.bareSessionId);
      }
    },
    [],
  );

  const counts = useMemo(() => {
    const c: Record<NodeKind, number> = { project: 0, topic: 0, session: 0 };
    for (const n of data.nodes) c[n.kind]++;
    return c;
  }, [data]);

  // react-force-graph sizes its canvas from its own wrapper <div>, which
  // collapses to height:0 inside this `absolute inset-0` layout — leaving
  // the graph invisible. Measure the container ourselves and feed explicit
  // pixel dimensions so the canvas always fills the available space (and
  // tracks window/panel resizes via ResizeObserver).
  const containerRef = useRef<HTMLDivElement>(null);
  const [dims, setDims] = useState({ width: 0, height: 0 });
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const update = () =>
      setDims({ width: el.clientWidth, height: el.clientHeight });
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  // Re-frame the graph when the canvas size changes so it isn't left
  // squashed off-center after a resize (the engine's onEngineStop fit only
  // fires while the simulation is still running).
  useEffect(() => {
    if (dims.width > 0 && dims.height > 0) {
      fgRef.current?.zoomToFit(400, 80);
    }
  }, [dims]);

  return (
    <div
      ref={containerRef}
      className="absolute inset-0"
      style={{ background: canvasBg() }}
    >
      <ForceGraph2D<GNode, GLink>
        ref={fgRef}
        width={dims.width || undefined}
        height={dims.height || undefined}
        graphData={data as { nodes: GNode[]; links: GLink[] }}
        backgroundColor={canvasBg()}
        nodeId="id"
        nodeRelSize={7}
        nodeVal={(n) => Math.max(1.5, n.size / 2.5)}
        // Use time-based cooldown so each `d3ReheatSimulation()` (fired
        // on slider drag) gets a fresh budget. The library doesn't reset
        // the `cooldownTicks` counter on reheat, so tick-based capping
        // would silently freeze the graph after the first 200 ticks.
        cooldownTime={20000}
        warmupTicks={20}
        d3AlphaDecay={0.018}
        d3VelocityDecay={0.32}
        onEngineStop={() => {
          // After the layout settles, frame the whole graph nicely
          // instead of leaving it squashed off-center.
          fgRef.current?.zoomToFit(700, 80);
        }}
        nodeVisibility={(n) => visibleIds.has(n.id as string)}
        linkVisibility={(l) => {
          const s = endpointId(l.source as string | NodeObject<GNode>);
          const t = endpointId(l.target as string | NodeObject<GNode>);
          return visibleIds.has(s) && visibleIds.has(t);
        }}
        linkColor={(l) => {
          const s = endpointId(l.source as string | NodeObject<GNode>);
          const t = endpointId(l.target as string | NodeObject<GNode>);
          if (!focusId) return COLOR_LINK_DEFAULT;
          return s === focusId || t === focusId
            ? COLOR_LINK_HIGHLIGHT
            : COLOR_LINK_DIM;
        }}
        linkWidth={(l) => {
          const s = endpointId(l.source as string | NodeObject<GNode>);
          const t = endpointId(l.target as string | NodeObject<GNode>);
          if (!focusId) return l.kind === 'similarity' ? 1.6 : 1.3;
          return s === focusId || t === focusId ? 2.4 : 0.5;
        }}
        linkDirectionalParticles={0}
        nodeCanvasObject={(node, ctx, globalScale) => {
          const n = node as NodeObject<GNode>;
          const x = (n.x ?? 0) as number;
          const y = (n.y ?? 0) as number;
          // Bigger dots overall while keeping the project > topic >
          // session size hierarchy intact.
          const baseRadius =
            n.kind === 'project'
              ? Math.max(10, Math.sqrt(n.size) * 3.3)
              : n.kind === 'topic'
                ? Math.max(6, Math.sqrt(n.size) * 2.5)
                : Math.max(4, Math.sqrt(n.size) * 1.8);
          const isFocus = focusId === n.id;
          const isNeighbor =
            !!focusId && (neighbors.get(focusId)?.has(n.id as string) ?? false);
          const dim = !!focusId && !isFocus && !isNeighbor;

          // Dot
          ctx.beginPath();
          ctx.arc(x, y, baseRadius, 0, Math.PI * 2);
          ctx.fillStyle = dim
            ? hexToRgba(COLOR[n.kind], 0.22)
            : isFocus
              ? COLOR_HIGHLIGHT
              : COLOR[n.kind];
          ctx.fill();

          // Focus halo
          if (isFocus) {
            ctx.beginPath();
            ctx.arc(x, y, baseRadius + 4, 0, Math.PI * 2);
            ctx.strokeStyle = 'rgba(167, 139, 250, 0.55)';
            ctx.lineWidth = 1.6;
            ctx.stroke();
          }

          // Labels are HIDDEN by default — only show during focus mode
          // (for the focused node + its neighbors) and when zoomed in
          // close enough that overlap isn't a problem.
          const showLabel = isFocus || isNeighbor || globalScale > 3.2;
          if (showLabel) {
            const fontSize = Math.max(3, 11 / globalScale);
            ctx.font = `${isFocus ? 'bold ' : ''}${fontSize}px "Plus Jakarta Sans", system-ui, sans-serif`;
            ctx.textAlign = 'center';
            ctx.textBaseline = 'top';
            const text = truncate(cleanLabel(n.label, n.kind), 28);
            const ly = y + baseRadius + 2;
            // White outline so colored text stays legible against the
            // light canvas no matter what's behind.
            ctx.strokeStyle = LABEL_OUTLINE;
            ctx.lineWidth = Math.max(1.8, 3 / globalScale);
            ctx.lineJoin = 'round';
            ctx.miterLimit = 2;
            ctx.strokeText(text, x, ly);
            ctx.fillStyle = LABEL_COLOR[n.kind];
            ctx.fillText(text, x, ly);
          }
        }}
        nodePointerAreaPaint={(node, color, ctx) => {
          const n = node as NodeObject<GNode>;
          const x = (n.x ?? 0) as number;
          const y = (n.y ?? 0) as number;
          ctx.fillStyle = color;
          ctx.beginPath();
          ctx.arc(x, y, Math.max(6, Math.sqrt(n.size) * 3.0), 0, Math.PI * 2);
          ctx.fill();
        }}
        onNodeClick={onNodeClick}
        onBackgroundClick={() => setFocusId(null)}
        onNodeHover={(node, _prev) => {
          const n = node as NodeObject<GNode> | null;
          if (!n) {
            setHovered(null);
            document.body.style.cursor = 'default';
            return;
          }
          document.body.style.cursor = 'pointer';
          const x = (n.x ?? 0) as number;
          const y = (n.y ?? 0) as number;
          // Convert sim coordinates → screen coordinates via the graph
          // helper so the tooltip follows even after zoom/pan.
          const screen = fgRef.current?.graph2ScreenCoords(x, y);
          if (!screen) return;
          setHovered({
            label: cleanLabel(n.label, n.kind),
            sub: subtitle(n),
            x: screen.x,
            y: screen.y,
          });
        }}
      />

      <AnimatePresence>
        {hovered && (
          <motion.div
            initial={{ opacity: 0, y: 6 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0 }}
            className="pointer-events-none absolute z-20 px-2.5 py-1.5 rounded-lg backdrop-blur text-[11px]"
            style={{
              left: hovered.x + 12,
              top: hovered.y + 12,
              maxWidth: 280,
              background: 'rgb(var(--ink-900) / 0.94)',
              border: '1px solid rgba(255, 255, 255, 0.12)',
              color: 'rgb(var(--ink-50))',
              boxShadow: '0 6px 24px rgba(0, 0, 0, 0.6)',
            }}
          >
            <div className="font-semibold leading-tight line-clamp-2">
              {hovered.label}
            </div>
            <div className="text-[10px] opacity-60 font-mono mt-0.5 whitespace-pre-line">
              {hovered.sub}
            </div>
          </motion.div>
        )}
      </AnimatePresence>

      <ControlsPanel
        showKinds={showKinds}
        onToggleKind={(k) =>
          setShowKinds((cur) => ({ ...cur, [k]: !cur[k] }))
        }
        forces={forces}
        onForces={setForces}
        counts={counts}
        search={search}
        onSearch={setSearch}
        allProjects={allProjects}
        selectedProjects={selectedProjects}
        onToggleProject={(id) =>
          setSelectedProjects((cur) => {
            const base = cur ?? new Set(allProjects.map((p) => p.id));
            const next = new Set(base);
            if (next.has(id)) next.delete(id);
            else next.add(id);
            return next;
          })
        }
        onClearProjects={() => setSelectedProjects(null)}
        dateStart={dateStart}
        dateEnd={dateEnd}
        onDateStart={setDateStart}
        onDateEnd={setDateEnd}
      />

      <SessionPanel
        sessionId={selectedSession}
        onClose={() => setSelectedSession(null)}
      />

      {/* Stats footer */}
      {graph.data && (
        <div
          className="absolute bottom-3 left-4 flex items-center gap-2 px-3 py-1.5 rounded-full backdrop-blur text-[10.5px] font-mono"
          style={{
            background: 'rgb(var(--ink-900) / 0.88)',
            border: '1px solid rgb(var(--ink-50) / 0.10)',
            color: 'rgb(var(--ink-200))',
            boxShadow: '0 2px 8px rgba(0, 0, 0, 0.5)',
          }}
        >
          <NetworkIcon className="w-3 h-3" style={{ color: COLOR.topic }} />
          <span>{counts.project}</span>
          <span className="opacity-60">projects</span>
          <span className="opacity-40">·</span>
          <span>{counts.topic}</span>
          <span className="opacity-60">topics</span>
          <span className="opacity-40">·</span>
          <span>{counts.session}</span>
          <span className="opacity-60">sessions</span>
          <span className="opacity-40">·</span>
          <Sparkles className="w-3 h-3" style={{ color: COLOR.project }} />
          <span>{graph.data.elapsed_ms}ms</span>
        </div>
      )}
    </div>
  );
}

function ControlsPanel({
  showKinds,
  onToggleKind,
  forces,
  onForces,
  counts,
  search,
  onSearch,
  allProjects,
  selectedProjects,
  onToggleProject,
  onClearProjects,
  dateStart,
  dateEnd,
  onDateStart,
  onDateEnd,
}: {
  showKinds: Record<NodeKind, boolean>;
  onToggleKind: (k: NodeKind) => void;
  forces: Forces;
  onForces: (f: Forces) => void;
  counts: Record<NodeKind, number>;
  search: string;
  onSearch: (v: string) => void;
  allProjects: { id: string; label: string }[];
  selectedProjects: Set<string> | null;
  onToggleProject: (id: string) => void;
  onClearProjects: () => void;
  dateStart: string;
  dateEnd: string;
  onDateStart: (v: string) => void;
  onDateEnd: (v: string) => void;
}) {
  const projectChecked = (id: string) =>
    selectedProjects === null || selectedProjects.has(id);
  const activeProjectCount =
    selectedProjects === null ? allProjects.length : selectedProjects.size;
  const hasDate = dateStart || dateEnd;

  // Collapsible projects dropdown with its own name search so the panel
  // stays compact when the user has dozens of indexed projects.
  const [projectsOpen, setProjectsOpen] = useState(false);
  const [projectsQuery, setProjectsQuery] = useState('');
  const filteredProjects = useMemo(() => {
    const q = projectsQuery.trim().toLowerCase();
    if (!q) return allProjects;
    return allProjects.filter(
      (p) =>
        p.label.toLowerCase().includes(q) ||
        p.id.toLowerCase().includes(q),
    );
  }, [allProjects, projectsQuery]);
  const [collapsed, setCollapsed] = useState(false);
  return (
    <motion.aside
      initial={{ x: 280 }}
      animate={{ x: collapsed ? 250 : 0 }}
      transition={{ type: 'spring', stiffness: 220, damping: 26 }}
      className="absolute top-3 right-3 w-[280px] rounded-2xl backdrop-blur-md z-10"
      style={{
        background: 'rgb(var(--ink-900) / 0.94)',
        border: '1px solid rgb(var(--ink-50) / 0.10)',
        boxShadow: '0 6px 24px rgba(0, 0, 0, 0.6)',
        color: 'rgb(var(--ink-100))',
      }}
    >
      <button
        onClick={() => setCollapsed((c) => !c)}
        className="absolute -left-7 top-3 w-6 h-6 rounded-l-lg grid place-items-center text-ink-300 hover:text-ink-50"
        style={{
          background: 'rgb(var(--ink-900) / 0.94)',
          border: '1px solid rgb(var(--ink-50) / 0.10)',
          borderRight: 'none',
        }}
        aria-label={collapsed ? 'expand controls' : 'collapse controls'}
      >
        <Sliders className="w-3 h-3" />
      </button>

      <div className="p-3 flex flex-col gap-3">

        {/* Search nodes — top of the panel for fast access */}
        <Section icon={<Search className="w-3 h-3" />} label="Search nodes">
          <div
            className="flex items-center gap-2 px-2 py-1.5 rounded-lg"
            style={{
              background: 'rgb(var(--ink-50) / 0.05)',
              border: '1px solid rgb(var(--ink-50) / 0.08)',
            }}
          >
            <Search className="w-3.5 h-3.5 text-ink-300" />
            <input
              type="search"
              value={search}
              onChange={(e) => onSearch(e.target.value)}
              placeholder="Search nodes…"
              className="bg-transparent outline-none text-[11.5px] text-ink-50 placeholder:text-ink-300 flex-1 min-w-0"
            />
            {search && (
              <button
                onClick={() => onSearch('')}
                className="text-ink-300 hover:text-ink-100"
                aria-label="clear node search"
              >
                <RotateCcw className="w-3 h-3" />
              </button>
            )}
          </div>
        </Section>

        {/* Node-type toggles */}
        <Section
          icon={<Filter className="w-3 h-3" />}
          label="Show types"
        >
          <FilterRow
            color={COLOR.project}
            icon={<FolderTree className="w-3 h-3" />}
            label="Projects"
            count={counts.project}
            on={showKinds.project}
            onToggle={() => onToggleKind('project')}
          />
          <FilterRow
            color={COLOR.topic}
            icon={<NetworkIcon className="w-3 h-3" />}
            label="Topics"
            count={counts.topic}
            on={showKinds.topic}
            onToggle={() => onToggleKind('topic')}
          />
          <FilterRow
            color={COLOR.session}
            icon={<MessageSquare className="w-3 h-3" />}
            label="Sessions"
            count={counts.session}
            on={showKinds.session}
            onToggle={() => onToggleKind('session')}
          />
        </Section>

        {/* Forces */}
        <Section icon={<Sliders className="w-3 h-3" />} label="Forces">
          <Slider
            label="Repel force"
            min={-1000}
            max={-10}
            step={5}
            value={forces.charge}
            display={`${-forces.charge}`}
            onChange={(v) => onForces({ ...forces, charge: v })}
          />
          <Slider
            label="Link force"
            min={0}
            max={1}
            step={0.02}
            value={forces.linkStrength}
            display={forces.linkStrength.toFixed(2)}
            onChange={(v) => onForces({ ...forces, linkStrength: v })}
          />
          <Slider
            label="Link distance"
            min={5}
            max={400}
            step={1}
            value={forces.linkDistance}
            display={`${forces.linkDistance}`}
            onChange={(v) => onForces({ ...forces, linkDistance: v })}
          />
          <Slider
            label="Center force"
            min={0}
            max={1}
            step={0.01}
            value={forces.center}
            display={forces.center.toFixed(2)}
            onChange={(v) => onForces({ ...forces, center: v })}
          />
          <button
            onClick={() => onForces(DEFAULT_FORCES)}
            className="mt-1 flex items-center gap-1 text-[10px] uppercase tracking-widest font-bold text-ink-300 hover:text-ink-50"
          >
            <RotateCcw className="w-3 h-3" />
            reset
          </button>
        </Section>

        {/* Projects — collapsible dropdown with name-search */}
        <div className="flex flex-col gap-1.5">
          <button
            onClick={() => setProjectsOpen((o) => !o)}
            className="flex items-center gap-1 text-[9px] uppercase tracking-widest font-bold text-ink-300 hover:text-ink-50 text-left"
          >
            <FolderTree className="w-3 h-3" />
            <span>Projects</span>
            <span className="font-mono opacity-60 normal-case tracking-normal text-[10px] ml-0.5">
              ({activeProjectCount}/{allProjects.length})
            </span>
            <ChevronDown
              className={`w-3 h-3 ml-auto transition-transform ${
                projectsOpen ? 'rotate-180' : ''
              }`}
            />
          </button>

          <AnimatePresence initial={false}>
            {projectsOpen && (
              <motion.div
                initial={{ height: 0, opacity: 0 }}
                animate={{ height: 'auto', opacity: 1 }}
                exit={{ height: 0, opacity: 0 }}
                transition={{ duration: 0.18, ease: 'easeOut' }}
                className="flex flex-col gap-1.5 overflow-hidden"
              >
                <div
                  className="flex items-center gap-2 px-2 py-1 rounded-lg"
                  style={{
                    background: 'rgb(var(--ink-50) / 0.05)',
                    border: '1px solid rgb(var(--ink-50) / 0.08)',
                  }}
                >
                  <Search className="w-3 h-3 text-ink-300" />
                  <input
                    type="search"
                    value={projectsQuery}
                    onChange={(e) => setProjectsQuery(e.target.value)}
                    placeholder="Search projects…"
                    className="bg-transparent outline-none text-[11px] text-ink-50 placeholder:text-ink-300 flex-1 min-w-0"
                  />
                  {projectsQuery && (
                    <button
                      onClick={() => setProjectsQuery('')}
                      className="text-ink-300 hover:text-ink-100"
                      aria-label="clear project search"
                    >
                      <RotateCcw className="w-3 h-3" />
                    </button>
                  )}
                </div>

                <div className="flex flex-col gap-0.5 max-h-[200px] overflow-y-auto ck-scroll-dark pr-1">
                  {filteredProjects.length === 0 ? (
                    <div className="px-2 py-1.5 text-[10.5px] text-ink-300 italic">
                      no matches
                    </div>
                  ) : (
                    filteredProjects.map((p) => {
                      const on = projectChecked(p.id);
                      return (
                        <button
                          key={p.id}
                          onClick={() => onToggleProject(p.id)}
                          className={`flex items-center gap-2 px-2 py-1 rounded-lg text-[11px] transition-colors ${
                            on
                              ? 'bg-ink-50/10 text-ink-50'
                              : 'bg-transparent text-ink-300 hover:bg-white/5'
                          }`}
                        >
                          <span
                            className="w-3 h-3 rounded grid place-items-center shrink-0"
                            style={{
                              background: on ? COLOR.project : 'transparent',
                              border: `1.5px solid ${COLOR.project}`,
                            }}
                          >
                            {on && (
                              <svg viewBox="0 0 12 12" className="w-2.5 h-2.5">
                                <path
                                  d="M2 6.5 L5 9.5 L10 3"
                                  stroke="rgb(var(--ink-950))"
                                  strokeWidth="2"
                                  fill="none"
                                  strokeLinecap="round"
                                  strokeLinejoin="round"
                                />
                              </svg>
                            )}
                          </span>
                          <span className="truncate flex-1 text-left font-semibold">
                            {p.label}
                          </span>
                        </button>
                      );
                    })
                  )}
                </div>

                {selectedProjects !== null && (
                  <button
                    onClick={onClearProjects}
                    className="flex items-center gap-1 text-[10px] uppercase tracking-widest font-bold text-ink-300 hover:text-ink-50"
                  >
                    <RotateCcw className="w-3 h-3" />
                    clear filter
                  </button>
                )}
              </motion.div>
            )}
          </AnimatePresence>
        </div>

        {/* Date range */}
        <Section icon={<CalendarRange className="w-3 h-3" />} label="Date range">
          <div className="flex flex-col gap-1.5">
            <DateField label="From" value={dateStart} onChange={onDateStart} />
            <DateField label="To" value={dateEnd} onChange={onDateEnd} />
            {hasDate && (
              <button
                onClick={() => {
                  onDateStart('');
                  onDateEnd('');
                }}
                className="mt-0.5 flex items-center gap-1 text-[10px] uppercase tracking-widest font-bold text-ink-300 hover:text-ink-50"
              >
                <RotateCcw className="w-3 h-3" />
                clear dates
              </button>
            )}
          </div>
        </Section>
      </div>
    </motion.aside>
  );
}


function DateField({
  label,
  value,
  onChange,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <label className="flex items-center gap-2">
      <span className="text-[10px] text-ink-300 font-semibold w-8 shrink-0">
        {label}
      </span>
      <input
        type="date"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="ck-date flex-1 min-w-0 px-2 py-1 rounded-md text-[11px] text-ink-50 outline-none"
        style={{
          background: 'rgb(var(--ink-50) / 0.05)',
          border: '1px solid rgb(var(--ink-50) / 0.08)',
          colorScheme: 'dark',
        }}
      />
    </label>
  );
}

function Section({
  icon,
  label,
  children,
}: {
  icon: React.ReactNode;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-center gap-1 text-[9px] uppercase tracking-widest font-bold text-ink-300">
        {icon}
        <span>{label}</span>
      </div>
      <div className="flex flex-col gap-1.5">{children}</div>
    </div>
  );
}

function FilterRow({
  color,
  icon,
  label,
  count,
  on,
  onToggle,
}: {
  color: string;
  icon: React.ReactNode;
  label: string;
  count: number;
  on: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      onClick={onToggle}
      className={`flex items-center gap-2 px-2 py-1 rounded-lg text-[11px] font-semibold transition-colors ${
        on
          ? 'bg-ink-50/10 text-ink-50'
          : 'bg-transparent text-ink-300 hover:bg-white/5'
      }`}
    >
      <span
        className="w-2.5 h-2.5 rounded-full shrink-0"
        style={{ background: on ? color : 'transparent', border: `1.5px solid ${color}` }}
      />
      <span className="opacity-80">{icon}</span>
      <span className="flex-1 text-left">{label}</span>
      <span className="text-[10px] opacity-60 font-mono">{count}</span>
    </button>
  );
}

function Slider({
  label,
  min,
  max,
  step,
  value,
  display,
  onChange,
}: {
  label: string;
  min: number;
  max: number;
  step: number;
  value: number;
  display: string;
  onChange: (v: number) => void;
}) {
  return (
    <label className="flex flex-col gap-0.5">
      <div className="flex items-center justify-between text-[10px] text-ink-300">
        <span className="font-semibold">{label}</span>
        <span className="font-mono opacity-70">{display}</span>
      </div>
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        className="ck-range-dark"
      />
    </label>
  );
}

// ---------- graph construction ----------

function buildGraph(
  rawNodes: GraphNode[],
  rawEdges: GraphEdge[],
): { nodes: GNode[]; links: GLink[] } {
  const nodes: GNode[] = [];
  const links: GLink[] = [];

  const sessionToProject = new Map<string, string>();
  const seen = new Set<string>();

  for (const r of rawNodes) {
    if (seen.has(r.id)) continue;
    seen.add(r.id);
    if (r.kind === 'project') {
      nodes.push({
        id: r.id,
        kind: 'project',
        label: prettyProject(r.label),
        size: 18,
        sessionCount: r.sessions,
        cwd: r.cwd ?? null,
      });
    } else if (r.kind === 'topic') {
      nodes.push({
        id: r.id,
        kind: 'topic',
        label: r.label,
        size: 9 + Math.min(r.size / 12, 6),
        sessionCount: r.session_ids.length,
        chunkCount: r.size,
      });
    } else if (r.kind === 'session') {
      const bareId = r.id.replace(/^[pts]:/, '');
      nodes.push({
        id: r.id,
        kind: 'session',
        label: r.label || r.ai_title || bareId,
        size: 4 + Math.min(r.message_count / 60, 5),
        projectId: r.project_id,
        bareSessionId: bareId,
        endedAt: r.ended_at,
      });
      sessionToProject.set(r.id, r.project_id);
    }
  }

  // Containment edges from the API (contains-topic, contains-session).
  for (const e of rawEdges) {
    if (e.kind === 'contains-topic' || e.kind === 'contains-session') {
      links.push({ source: e.from, target: e.to, kind: 'contains' });
    } else if (e.kind === 'topic-similarity') {
      links.push({ source: e.from, target: e.to, kind: 'similarity' });
    }
  }

  // Backfill: any session not connected via a topic gets a direct link
  // to its project so it stays attached to its project's cluster.
  const linkedSessions = new Set<string>();
  for (const l of links) {
    if (typeof l.source === 'string' && l.source.startsWith('s:'))
      linkedSessions.add(l.source);
    if (typeof l.target === 'string' && l.target.startsWith('s:'))
      linkedSessions.add(l.target);
  }
  for (const [sid, pid] of sessionToProject) {
    if (!linkedSessions.has(sid)) {
      // The project node id from the API is project_id (raw), not
      // prefixed. Make sure it exists as a node before linking.
      if (!nodes.find((n) => n.id === pid)) {
        nodes.push({
          id: pid,
          kind: 'project',
          label: prettyProject(pid),
          size: 18,
        });
      }
      links.push({ source: pid, target: sid, kind: 'contains' });
    }
  }

  return { nodes, links };
}

function cleanLabel(s: string, kind: NodeKind): string {
  if (kind === 'topic') {
    return s
      .replace(/^Tool call:\s*/i, '')
      .replace(/\s+Input:.*$/i, '')
      .trim();
  }
  return s;
}

function truncate(s: string, max: number): string {
  return s.length > max ? s.slice(0, max - 1) + '…' : s;
}

function subtitle(n: NodeObject<GNode>): string {
  if (n.kind === 'project') {
    const head = `project · ${n.sessionCount ?? 0} sessions`;
    return n.cwd ? `${head}\n${n.cwd}` : head;
  }
  if (n.kind === 'topic')
    return `topic · ${n.sessionCount ?? 0} sessions · ${n.chunkCount ?? 0} chunks`;
  if (n.kind === 'session')
    return `session · ${n.projectId ? prettyProject(n.projectId) : ''}`;
  return '';
}

function hexToRgba(hex: string, alpha: number): string {
  const m = hex.replace('#', '');
  const r = parseInt(m.slice(0, 2), 16);
  const g = parseInt(m.slice(2, 4), 16);
  const b = parseInt(m.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}
