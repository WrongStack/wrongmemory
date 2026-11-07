import { History, Radio } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { read } from '../api';
import { absTime, humanize, relTime, shortId } from '../format';
import type { AuditRecord, ChangeEvent } from '../types';
import { Chip, Empty, ErrorNote, Panel, Spinner, useInterval, useLoad } from '../ui';
import type { ProjectCtx } from './ProjectView';

const CHANGE_TONE: Record<string, string> = {
  'memory.accepted': 'ok',
  'memory.merged': 'info',
  'memory.updated': 'info',
  'memory.deleted': 'error',
  'memory.recovered': 'ok',
};

export function ActivityTab({ ctx }: { ctx: ProjectCtx }) {
  const pid = ctx.project.projectId;
  const [events, setEvents] = useState<ChangeEvent[]>([]);
  const [error, setError] = useState<unknown>();
  const cursor = useRef<{ gen: number; instance?: string }>({ gen: 0 });
  const audit = useLoad(() => read<{ records: AuditRecord[] }>('memory.audit', pid, { limit: 200 }), [pid, ctx.version]);

  async function poll() {
    try {
      const r = await read<{ events: ChangeEvent[]; generation: number; daemonInstanceId: string; resync?: boolean }>('events.since', pid, {
        generation: cursor.current.gen,
        daemonInstanceId: cursor.current.instance,
      });
      const reset = r.daemonInstanceId !== cursor.current.instance || r.resync;
      cursor.current = { gen: r.generation, instance: r.daemonInstanceId };
      setEvents((old) => [...[...r.events].reverse(), ...(reset ? [] : old)].slice(0, 300));
      setError(undefined);
    } catch (e) {
      setError(e);
    }
  }
  // biome-ignore lint/correctness/useExhaustiveDependencies: first poll only
  useEffect(() => {
    void poll();
  }, [pid]);
  useInterval(poll, 3000);

  const visible = events.filter((e) => !e.telemetry);

  return (
    <div className="mx-auto grid max-w-6xl gap-5 lg:grid-cols-2">
      <Panel
        title={
          <span className="flex items-center gap-2">
            <Radio className="size-4 text-ok" /> Live changes
          </span>
        }
        actions={<span className="text-xs text-faint">refreshes every 3 s</span>}
      >
        {error !== undefined && <ErrorNote error={error} />}
        {!visible.length ? (
          <p className="text-[13px] text-muted">No changes since this page opened. They appear here as agents write.</p>
        ) : (
          <ul className="flex flex-col">
            {visible.map((e, i) => (
              <li key={`${e.generation}-${i}`} className="flex items-center gap-2 border-b border-line py-2 text-[13px] last:border-0">
                <Chip tone={CHANGE_TONE[e.change]}>{humanize(e.change.replace(/^memory\.|^candidate\./, ''))}</Chip>
                {e.memoryId ? (
                  <button type="button" className="font-mono text-xs text-accent hover:underline" onClick={() => ctx.openMemory(e.memoryId as string)}>
                    {shortId(e.memoryId)}
                  </button>
                ) : (
                  <span className="font-mono text-xs text-muted">{shortId(e.candidateId)}</span>
                )}
                {e.revision !== undefined && <span className="text-xs text-faint">rev {e.revision}</span>}
                <span className="ml-auto text-xs text-faint" title={absTime(e.atMs)}>
                  {e.origin ? 'client' : 'daemon'} · {relTime(e.atMs)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </Panel>

      <Panel
        title={
          <span className="flex items-center gap-2">
            <History className="size-4 text-muted" /> Audit trail
          </span>
        }
        actions={<span className="text-xs text-faint">stored in the project</span>}
      >
        {audit.error ? (
          <ErrorNote error={audit.error} />
        ) : !audit.data ? (
          <Spinner />
        ) : !audit.data.records.length ? (
          <Empty title="The audit trail is empty" />
        ) : (
          <ul className="flex max-h-[36rem] flex-col overflow-y-auto">
            {audit.data.records.map((a, i) => (
              <li key={i} className="border-b border-line py-2 text-[13px] last:border-0">
                <div className="flex items-center gap-2">
                  <span className="font-medium">{humanize(a.event.replace(/^memory\./, ''))}</span>
                  {a.memoryId && (
                    <button type="button" className="font-mono text-xs text-accent hover:underline" onClick={() => ctx.openMemory(a.memoryId as string)}>
                      {shortId(a.memoryId)}
                    </button>
                  )}
                  <span className="ml-auto text-xs text-faint" title={absTime(a.at ?? a.createdAt)}>
                    {relTime(a.at ?? a.createdAt)}
                  </span>
                </div>
                {a.reason && <div className="mt-0.5 line-clamp-2 text-xs text-muted">{a.reason}</div>}
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}
