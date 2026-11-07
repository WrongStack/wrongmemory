import {
  Background,
  BackgroundVariant,
  Controls,
  type Edge,
  Handle,
  MarkerType,
  MiniMap,
  type Node,
  type NodeProps,
  Position,
  ReactFlow,
} from '@xyflow/react';
import { Bot, FileCode, Folder, Hash, Network, Package, Search, Terminal, Wrench } from 'lucide-react';
import { type FormEvent, useEffect, useMemo, useState } from 'react';
import { read } from '../api';
import { humanize, preview } from '../format';
import type { GraphEdge, GraphView, Memory } from '../types';
import { Button, Chip, cx, Empty, ErrorNote, KindIcon, Spinner, StatusChip, useLoad } from '../ui';
import type { ProjectCtx } from './ProjectView';

const MEM_W = 250;
const NODE_W = 210;
const COL_GAP = 150;
const ROW_H = 150;
const STRUCT_ROW_H = 64;
const MAX_PER_COL = 12;

interface MemData extends Record<string, unknown> {
  m: Memory;
  focus: boolean;
}
interface StructData extends Record<string, unknown> {
  id: string;
  focus: boolean;
}

function structIcon(id: string) {
  const cls = 'size-3.5 shrink-0';
  if (id.startsWith('dir:')) return <Folder className={cls} />;
  if (id.startsWith('symbol:')) return <Hash className={cls} />;
  if (id.startsWith('command:')) return <Terminal className={cls} />;
  if (id.startsWith('package:')) return <Package className={cls} />;
  if (id.startsWith('tool:')) return <Wrench className={cls} />;
  if (id.startsWith('agent:')) return <Bot className={cls} />;
  return <FileCode className={cls} />;
}

function structLabel(id: string): { type: string; label: string } {
  const i = id.indexOf(':');
  return i < 0 ? { type: 'node', label: id } : { type: id.slice(0, i), label: id.slice(i + 1) };
}

function MemoryNode({ data }: NodeProps<Node<MemData, 'mem'>>) {
  const { m, focus } = data;
  return (
    <div
      className={cx(
        'cursor-pointer rounded-lg border bg-panel px-3 py-2.5 shadow-lg transition-colors hover:border-accent',
        focus ? 'border-accent shadow-[0_0_0_3px_var(--accent-soft)]' : 'border-line',
      )}
      style={{ width: MEM_W }}
      title={m.text}
    >
      <Handle type="target" position={Position.Left} />
      <Handle type="source" position={Position.Right} />
      <div className="flex items-center gap-2">
        <KindIcon kind={m.kind} size="sm" />
        <span className="truncate text-[11px] font-medium text-muted">{humanize(m.kind)}</span>
        {m.status !== 'active' && (
          <span className="ml-auto">
            <StatusChip status={m.status} />
          </span>
        )}
      </div>
      <p className="mt-1.5 line-clamp-3 text-[12px] leading-[1.35rem] text-fg">{preview(m.text, 170)}</p>
    </div>
  );
}

function StructNode({ data }: NodeProps<Node<StructData, 'struct'>>) {
  const { type, label } = structLabel(data.id);
  return (
    <div
      className={cx(
        'flex cursor-pointer items-center gap-2 rounded-full border bg-info-soft px-3 py-1.5 text-info shadow transition-colors hover:border-info',
        data.focus ? 'border-info shadow-[0_0_0_3px_var(--info-soft)]' : 'border-info/30',
      )}
      style={{ width: NODE_W }}
      title={`${type}: ${label} — click to centre here`}
    >
      <Handle type="target" position={Position.Left} />
      <Handle type="source" position={Position.Right} />
      {structIcon(data.id)}
      <span className="truncate font-mono text-[11px] text-fg">{label}</span>
    </div>
  );
}

const nodeTypes = { mem: MemoryNode, struct: StructNode };

function edgeStyle(e: GraphEdge): Partial<Edge> {
  const r = e.relation;
  // Folder/file/symbol structure: quiet lines, no label.
  if (!e.from.startsWith('mem:') && !e.to.startsWith('mem:')) {
    return { style: { stroke: 'var(--line-strong)', strokeWidth: 1, opacity: 0.8 } };
  }
  const color =
    r === 'supersedes'
      ? 'var(--warn)'
      : r === 'contradicts'
        ? 'var(--bad)'
        : r.startsWith('about_')
          ? 'var(--info)'
          : r === 'related_to' || r === 'same_topic'
            ? 'var(--accent)'
            : 'var(--line-strong)';
  const strong = r === 'supersedes' || r === 'contradicts';
  return {
    style: { stroke: color, strokeWidth: strong ? 2 : 1.25, strokeDasharray: r === 'same_topic' || r === 'related_to' ? '5 4' : undefined, opacity: strong ? 1 : 0.75 },
    markerEnd: { type: MarkerType.ArrowClosed, color, width: 14, height: 14 },
    label: r.startsWith('about_') ? undefined : humanize(r),
    animated: r === 'contradicts',
  };
}

/** Left-to-right layers by distance from the focus, memories first in each layer. */
function layout(view: GraphView, focus: string): { nodes: Node[]; edges: Edge[] } {
  const mems = new Map(view.memories.map((m) => [`mem:${m.id}`, m]));
  const ids = new Set<string>(mems.keys());
  for (const e of view.edges) {
    ids.add(e.from);
    ids.add(e.to);
  }
  // Hidden or missing memories are dropped rather than drawn as blanks.
  for (const id of [...ids]) if (id.startsWith('mem:') && !mems.has(id)) ids.delete(id);
  const edges = view.edges.filter((e) => ids.has(e.from) && ids.has(e.to));

  const adj = new Map<string, string[]>();
  for (const id of ids) adj.set(id, []);
  for (const e of edges) {
    adj.get(e.from)?.push(e.to);
    adj.get(e.to)?.push(e.from);
  }
  const f = focus.trim().replace(/\\/g, '/');
  let roots = [...ids].filter((id) => id === f || id === `file:${f}` || id === `dir:${f}` || id === `mem:${f}`);
  if (!roots.length) {
    const best = [...ids].sort((a, b) => (adj.get(b)?.length ?? 0) - (adj.get(a)?.length ?? 0))[0];
    roots = best ? [best] : [];
  }
  const depth = new Map<string, number>();
  const queue = [...roots];
  for (const r of roots) depth.set(r, 0);
  while (queue.length) {
    const cur = queue.shift() as string;
    for (const n of adj.get(cur) ?? []) {
      if (!depth.has(n)) {
        depth.set(n, (depth.get(cur) ?? 0) + 1);
        queue.push(n);
      }
    }
  }
  const maxDepth = Math.max(0, ...depth.values());
  for (const id of ids) if (!depth.has(id)) depth.set(id, maxDepth + 1);

  const layers = new Map<number, string[]>();
  for (const [id, d] of depth) layers.set(d, [...(layers.get(d) ?? []), id]);
  const rank = (id: string) => (mems.has(id) ? 0 : 1);
  const nodes: Node[] = [];
  let x = 0;
  for (const d of [...layers.keys()].sort((a, b) => a - b)) {
    const layer = (layers.get(d) ?? []).sort(
      (a, b) => rank(a) - rank(b) || (mems.get(b)?.importance ?? 0) - (mems.get(a)?.importance ?? 0) || a.localeCompare(b),
    );
    for (let start = 0; start < layer.length; start += MAX_PER_COL) {
      const col = layer.slice(start, start + MAX_PER_COL);
      const heights = col.map((id) => (mems.has(id) ? ROW_H : STRUCT_ROW_H));
      const total = heights.reduce((a, b) => a + b, 0);
      let y = -total / 2;
      const width = col.some((id) => mems.has(id)) ? MEM_W : NODE_W;
      col.forEach((id, i) => {
        const m = mems.get(id);
        nodes.push(
          m
            ? { id, type: 'mem', position: { x, y }, data: { m, focus: roots.includes(id) } }
            : { id, type: 'struct', position: { x, y }, data: { id, focus: roots.includes(id) } },
        );
        y += heights[i];
      });
      x += width + COL_GAP;
    }
  }
  return {
    nodes,
    // Lines always run left to right (handles sit on the left and right);
    // when the relation points backwards, the arrow moves to the start.
    edges: edges.map((e) => {
      const back = (depth.get(e.from) ?? 0) > (depth.get(e.to) ?? 0);
      const style = edgeStyle(e);
      return {
        id: `${e.from}|${e.relation}|${e.to}`,
        source: back ? e.to : e.from,
        target: back ? e.from : e.to,
        ...style,
        markerEnd: back ? undefined : style.markerEnd,
        markerStart: back ? style.markerEnd : undefined,
      };
    }),
  };
}

const LEGEND: [string, string][] = [
  ['Replaces', 'var(--warn)'],
  ['Contradicts', 'var(--bad)'],
  ['Same topic / related', 'var(--accent)'],
  ['Anchored to file, symbol…', 'var(--info)'],
];

export function GraphTab({ ctx, focus }: { ctx: ProjectCtx; focus?: string }) {
  const pid = ctx.project.projectId;
  const [query, setQuery] = useState(focus ?? '');
  const [active, setActive] = useState(focus ?? '');
  const [depth, setDepth] = useState(2);

  // No focus yet: start from a memory that is anchored somewhere, so the
  // first view shows links rather than a lone card.
  const seed = useLoad(async () => {
    if (active) return null;
    const page = (await read<{ memories: Memory[] }>('memory.listPage', pid, { limit: 50 })).memories;
    return page.find((m) => (m.anchors ?? []).length > 0) ?? page[0] ?? null;
  }, [pid, active]);
  useEffect(() => {
    if (!active && seed.data) {
      setActive(`mem:${seed.data.id}`);
      setQuery(`mem:${seed.data.id}`);
    }
  }, [active, seed.data]);
  useEffect(() => {
    if (focus) {
      setActive(focus);
      setQuery(focus);
    }
  }, [focus]);

  const graph = useLoad(async () => {
    if (!active) return null;
    const view = await read<GraphView>('memory.graph', pid, { query: active, maxDepth: depth, limit: 120 });
    // A memory with no links still gets its own card.
    if (!view.memories.length && !view.edges.length && active.startsWith('mem:')) {
      const m = await read<Memory>('memory.get', pid, { id: active.slice(4) }).catch(() => null);
      if (m) return { ...view, memories: [m] };
    }
    return view;
  }, [pid, active, depth, ctx.version]);
  const flow = useMemo(() => (graph.data ? layout(graph.data, active) : null), [graph.data, active]);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setActive(query.trim());
  };

  return (
    <div className="flex h-[calc(100vh-15rem)] min-h-[32rem] flex-col gap-3">
      <form onSubmit={submit} className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-72 flex-1">
          <Search className="pointer-events-none absolute left-2.5 top-2 size-4 text-faint" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Centre on a topic, a file path (src/app.ts) or a memory (mem:ID)…"
            className="h-8 w-full rounded-md border border-line bg-panel pl-8 pr-3 text-[13px] outline-none focus:border-accent"
          />
        </div>
        <label className="flex items-center gap-2 text-[13px] text-muted">
          Depth
          <select value={depth} onChange={(e) => setDepth(Number(e.target.value))} className="h-8 rounded-md border border-line bg-panel px-2 outline-none">
            {[1, 2, 3].map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        </label>
        <Button type="submit" tone="primary">
          Show
        </Button>
      </form>
      <div className="flex flex-wrap items-center gap-3 text-xs text-muted">
        {LEGEND.map(([label, color]) => (
          <span key={label} className="flex items-center gap-1.5">
            <span className="inline-block h-0.5 w-5 rounded" style={{ background: color }} /> {label}
          </span>
        ))}
        <span className="text-faint">· Click a memory to open it, a file or symbol to centre on it.</span>
        {graph.data?.truncated && <Chip tone="warn">Truncated at 120 nodes</Chip>}
      </div>

      <div className="relative min-h-0 flex-1 overflow-hidden rounded-lg border border-line bg-panel-2">
        {graph.error ? (
          <div className="p-4">
            <ErrorNote error={graph.error} />
          </div>
        ) : !flow ? (
          <div className="p-4">{seed.loading || graph.loading ? <Spinner /> : <Empty icon={<Network className="size-8" />} title="Nothing to show yet" />}</div>
        ) : !flow.nodes.length ? (
          <div className="p-4">
            <Empty icon={<Network className="size-8" />} title="No relations found">
              Nothing visible is linked to “{active}”. Try a file path, a different topic, or a larger depth.
            </Empty>
          </div>
        ) : (
          <ReactFlow
            key={`${active}|${depth}`}
            nodes={flow.nodes}
            edges={flow.edges}
            nodeTypes={nodeTypes}
            fitView
            fitViewOptions={{ padding: 0.12, maxZoom: 1.15 }}
            minZoom={0.15}
            maxZoom={2}
            nodesConnectable={false}
            proOptions={{ hideAttribution: true }}
            // Fit again once every card has its measured size.
            onInit={(flowApi) => setTimeout(() => void flowApi.fitView({ padding: 0.12, maxZoom: 1.15 }), 60)}
            onNodeClick={(_, n) => {
              if (n.type === 'mem') ctx.openMemory(n.id.slice(4));
              else {
                setActive(n.id);
                setQuery(n.id);
              }
            }}
          >
            <Background variant={BackgroundVariant.Dots} gap={20} size={1} color="var(--line)" />
            <Controls showInteractive={false} />
            {flow.nodes.length > 8 && (
              <MiniMap
                pannable
                zoomable
                bgColor="var(--panel)"
                maskColor="var(--accent-soft)"
                nodeColor={(n) => (n.type === 'mem' ? 'var(--accent)' : 'var(--info)')}
                className="!rounded-md !border !border-line"
              />
            )}
          </ReactFlow>
        )}
        {graph.loading && flow && <div className="absolute right-3 top-3 rounded-md bg-panel px-2 py-1 text-xs text-muted shadow">Updating…</div>}
      </div>
    </div>
  );
}
