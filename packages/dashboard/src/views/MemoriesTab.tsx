import { Brain, FileCode, Search } from 'lucide-react';
import { useEffect, useState } from 'react';
import { read } from '../api';
import { humanize, preview, relTime, absTime } from '../format';
import type { Memory, Stats } from '../types';
import { Button, Chip, Empty, ErrorNote, KindIcon, Meter, Spinner, StatusChip } from '../ui';
import type { ProjectCtx } from './ProjectView';

export const STATUSES = ['active', 'stale', 'archived', 'superseded', 'contradicted', 'deleted'];

export function anchorLabel(a: Memory['anchors'][number]): string {
  if (a.type === 'symbol') return `${a.path ?? ''}${a.symbol ? `#${a.symbol}` : ''}`;
  return a.path ?? a.command ?? a.symbol ?? a.role ?? a.type;
}

export function MemoryCard({ m, onOpen }: { m: Memory; onOpen: () => void }) {
  const anchors = m.anchors ?? [];
  return (
    <button
      type="button"
      onClick={onOpen}
      className="group flex w-full items-start gap-3 rounded-lg border border-line bg-panel px-4 py-3 text-left transition-colors hover:border-line-strong hover:bg-panel-2"
    >
      <KindIcon kind={m.kind} />
      <div className="min-w-0 flex-1">
        <p className="line-clamp-2 text-[13.5px] leading-5">{preview(m.text, 320)}</p>
        <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted">
          <span className="font-medium text-fg/80">{humanize(m.kind)}</span>
          {m.status !== 'active' && <StatusChip status={m.status} />}
          {anchors.slice(0, 2).map((a, i) => (
            <Chip key={i} className="max-w-64 font-mono">
              <FileCode className="size-3 shrink-0" />
              <span className="truncate">{anchorLabel(a)}</span>
            </Chip>
          ))}
          {anchors.length > 2 && <span className="text-faint">+{anchors.length - 2}</span>}
          {(m.tags ?? []).slice(0, 3).map((t) => (
            <span key={t} className="text-faint">
              #{t}
            </span>
          ))}
        </div>
      </div>
      <div className="hidden shrink-0 flex-col items-end gap-1.5 text-xs text-muted sm:flex">
        <span title={absTime(m.updatedAt)}>{relTime(m.updatedAt)}</span>
        <span className="flex items-center gap-1.5" title={`Importance ${m.importance.toFixed(2)}`}>
          <Meter value={m.importance} />
        </span>
        {(m.useCount ?? 0) > 0 && <span className="text-faint">used {m.useCount}×</span>}
      </div>
    </button>
  );
}

export function MemoriesTab({ ctx, stats }: { ctx: ProjectCtx; stats?: Stats }) {
  const pid = ctx.project.projectId;
  const [query, setQuery] = useState('');
  const [debounced, setDebounced] = useState('');
  const [status, setStatus] = useState('');
  const [kind, setKind] = useState('');
  const [rows, setRows] = useState<Memory[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [total, setTotal] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<unknown>();

  useEffect(() => {
    const t = setTimeout(() => setDebounced(query.trim()), 250);
    return () => clearTimeout(t);
  }, [query]);

  const statuses = status === 'all' ? STATUSES : status ? [status] : undefined;

  async function load(reset: boolean) {
    setLoading(true);
    try {
      if (debounced) {
        const r = await read<{ hits: { memory: Memory }[]; truncated?: boolean }>('memory.search', pid, {
          query: debounced,
          limit: 60,
          statuses,
          kinds: kind ? [kind] : undefined,
        });
        setRows(r.hits.map((h) => h.memory));
        setCursor(null);
        setTotal(null);
      } else {
        const r = await read<{ memories: Memory[]; nextCursor?: string; total: number }>('memory.listPage', pid, {
          limit: 50,
          cursor: reset ? undefined : (cursor ?? undefined),
          statuses,
          kind: kind || undefined,
        });
        setRows((old) => (reset ? r.memories : [...old, ...r.memories]));
        setCursor(r.nextCursor ?? null);
        setTotal(r.total);
      }
      setError(undefined);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
  }

  // biome-ignore lint/correctness/useExhaustiveDependencies: reload on filter or change
  useEffect(() => {
    void load(true);
  }, [debounced, status, kind, pid, ctx.version]);

  const kinds = Object.keys(stats?.byKind ?? {}).sort();
  const select = 'h-8 rounded-md border border-line bg-panel px-2 text-[13px] outline-none focus:border-accent';

  return (
    <div className="mx-auto flex max-w-5xl flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-64 flex-1">
          <Search className="pointer-events-none absolute left-2.5 top-2 size-4 text-faint" />
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search memories by meaning, file or words…"
            className="h-8 w-full rounded-md border border-line bg-panel pl-8 pr-3 text-[13px] outline-none focus:border-accent"
          />
        </div>
        <select aria-label="Status" value={status} onChange={(e) => setStatus(e.target.value)} className={select}>
          <option value="">Active</option>
          <option value="all">All statuses</option>
          {STATUSES.map((s) => (
            <option key={s} value={s}>
              {humanize(s)}
            </option>
          ))}
        </select>
        <select aria-label="Kind" value={kind} onChange={(e) => setKind(e.target.value)} className={select}>
          <option value="">All kinds</option>
          {kinds.map((k) => (
            <option key={k} value={k}>
              {humanize(k)} ({stats?.byKind?.[k]})
            </option>
          ))}
        </select>
      </div>
      <div className="text-xs text-muted">
        {debounced ? `${rows.length} matches` : total !== null ? `Showing ${rows.length.toLocaleString()} of ${total.toLocaleString()}` : ''}
        <span className="text-faint"> · Session-private memories are not shown here.</span>
      </div>

      {error ? (
        <ErrorNote error={error} />
      ) : loading && !rows.length ? (
        <Spinner />
      ) : !rows.length ? (
        <Empty icon={<Brain className="size-8" />} title={debounced ? 'No matches' : 'No memories'}>
          {debounced ? 'Try other words, a file path, or a wider status filter.' : 'Agents write memories here as they work.'}
        </Empty>
      ) : (
        <div className="flex flex-col gap-2">
          {rows.map((m) => (
            <MemoryCard key={m.id} m={m} onOpen={() => ctx.openMemory(m.id)} />
          ))}
        </div>
      )}
      {cursor && !debounced && (
        <div className="flex justify-center">
          <Button busy={loading} onClick={() => load(false)}>
            Load more
          </Button>
        </div>
      )}
    </div>
  );
}
