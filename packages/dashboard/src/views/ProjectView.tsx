import { Activity, Brain, HeartPulse, Inbox, LayoutGrid, Network, Trash2 } from 'lucide-react';
import { useState } from 'react';
import { read } from '../api';
import { statsOf, useApp } from '../App';
import { absTime, every, humanize, relTime, sum } from '../format';
import type { Job, Project, Schedule, Stats } from '../types';
import { Button, Chip, cx, ErrorNote, Panel, Spinner, Stat, StatusChip, useLoad } from '../ui';
import { ActivityTab } from './ActivityTab';
import { GraphTab } from './GraphTab';
import { HealthTab } from './HealthTab';
import { MemoryDrawer } from './MemoryDrawer';
import { MemoriesTab } from './MemoriesTab';
import { RemoveProjectsDialog } from './RemoveProjects';
import { ReviewTab } from './ReviewTab';

export type ProjectTab = 'overview' | 'memories' | 'graph' | 'review' | 'activity' | 'health';

const TABS: { id: ProjectTab; label: string; icon: typeof Brain }[] = [
  { id: 'overview', label: 'Overview', icon: LayoutGrid },
  { id: 'memories', label: 'Memories', icon: Brain },
  { id: 'graph', label: 'Relations', icon: Network },
  { id: 'review', label: 'Review', icon: Inbox },
  { id: 'activity', label: 'Activity', icon: Activity },
  { id: 'health', label: 'Health & maintenance', icon: HeartPulse },
];

export interface ProjectCtx {
  project: Project;
  openMemory: (id: string) => void;
  showInGraph: (focus: string) => void;
  /** Bumped after a change so tabs reload. */
  version: number;
  changed: () => void;
}

export function ProjectView({ pid, tab, focus }: { pid: string; tab: ProjectTab; focus?: string }) {
  const { projects, stats, go, refreshProjects } = useApp();
  const project = projects.find((p) => p.projectId === pid);
  const [memoryId, setMemoryId] = useState<string | null>(null);
  const [removing, setRemoving] = useState(false);
  const [version, setVersion] = useState(0);
  if (!project) return <div className="p-8"><Spinner /></div>;

  const st = statsOf(stats[pid]);
  const ctx: ProjectCtx = {
    project,
    openMemory: setMemoryId,
    showInGraph: (f) => {
      setMemoryId(null);
      go({ view: 'project', pid, tab: 'graph', focus: f });
    },
    version,
    changed: () => {
      setVersion((v) => v + 1);
      void refreshProjects().catch(() => {});
    },
  };

  return (
    <div className="flex min-h-full flex-col">
      <div className="border-b border-line bg-panel px-6 pt-5">
        <div className="flex flex-wrap items-start gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-2">
              <h1 className="truncate text-xl font-semibold">{project.displayName || project.projectId}</h1>
              <Chip>{project.storeKind}</Chip>
              <Chip tone={project.admission} title={project.admissionReason}>
                {project.admission}
              </Chip>
            </div>
            <div className="mt-1 truncate font-mono text-xs text-faint" title={project.root}>
              {project.root}
            </div>
          </div>
          <Button tone="danger" size="sm" icon={<Trash2 className="size-3.5" />} onClick={() => setRemoving(true)}>
            Remove project
          </Button>
        </div>
        {project.admission === 'read-only' && (
          <div className="mt-3 rounded-md border border-warn/40 bg-warn-soft px-3 py-2 text-[13px] text-warn">
            Read-only: {project.admissionReason ?? 'WrongMemory does not own writes to this store.'} Searches work; nothing can be written
            until write ownership is granted (<code className="font-mono">wrongmemory project add &lt;root&gt; --write</code>).
          </div>
        )}
        <nav className="mt-4 flex gap-1 overflow-x-auto" role="tablist">
          {TABS.map((t) => {
            const count = t.id === 'review' ? st?.pendingCandidates : t.id === 'memories' ? sum(st?.byStatus) : undefined;
            return (
              <button
                key={t.id}
                type="button"
                role="tab"
                aria-selected={tab === t.id}
                onClick={() => go({ view: 'project', pid, tab: t.id })}
                className={cx(
                  'flex items-center gap-1.5 whitespace-nowrap border-b-2 px-3 pb-2.5 pt-1 text-[13px] transition-colors',
                  tab === t.id ? 'border-accent text-fg' : 'border-transparent text-muted hover:text-fg',
                )}
              >
                <t.icon className="size-4" />
                {t.label}
                {!!count && <span className="rounded bg-raised px-1.5 text-[11px] tabular-nums text-muted">{count.toLocaleString()}</span>}
              </button>
            );
          })}
        </nav>
      </div>

      <div className="flex-1 p-6">
        {tab === 'overview' && <OverviewTab ctx={ctx} stats={st} />}
        {tab === 'memories' && <MemoriesTab ctx={ctx} stats={st} />}
        {tab === 'graph' && <GraphTab ctx={ctx} focus={focus} />}
        {tab === 'review' && <ReviewTab ctx={ctx} />}
        {tab === 'activity' && <ActivityTab ctx={ctx} />}
        {tab === 'health' && <HealthTab ctx={ctx} />}
      </div>

      {memoryId && <MemoryDrawer key={memoryId} ctx={ctx} id={memoryId} onClose={() => setMemoryId(null)} />}
      {removing && <RemoveProjectsDialog projects={[project]} onClose={() => setRemoving(false)} onDone={() => go({ view: 'home' })} />}
    </div>
  );
}

function Bars({ data, onPick }: { data?: Record<string, number>; onPick?: (k: string) => void }) {
  const entries = Object.entries(data ?? {})
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1]);
  if (!entries.length) return <p className="text-[13px] text-muted">Nothing yet.</p>;
  const max = Math.max(...entries.map(([, n]) => n));
  return (
    <div className="flex flex-col gap-1.5">
      {entries.map(([k, n]) => (
        <button
          key={k}
          type="button"
          disabled={!onPick}
          onClick={() => onPick?.(k)}
          className="group grid grid-cols-[9rem_1fr_4rem] items-center gap-3 rounded px-1 py-0.5 text-left text-[13px] enabled:hover:bg-raised"
        >
          <span className="truncate text-muted group-enabled:group-hover:text-fg">{humanize(k)}</span>
          <span className="h-2 overflow-hidden rounded-full bg-raised">
            <span className="block h-full rounded-full bg-accent/80" style={{ width: `${(100 * n) / max}%` }} />
          </span>
          <span className="text-right tabular-nums">{n.toLocaleString()}</span>
        </button>
      ))}
    </div>
  );
}

function OverviewTab({ ctx, stats }: { ctx: ProjectCtx; stats?: Stats }) {
  const pid = ctx.project.projectId;
  const { go } = useApp();
  const sched = useLoad(
    () => read<{ schedule?: Schedule; recentJobs?: Job[] }>('maintenance.status', pid),
    [pid, ctx.version],
  );
  const sem = useLoad(() => read<{ provider?: string; indexed?: number }>('semantic.status', pid), [pid]);
  const s = sched.data?.schedule;
  const total = sum(stats?.byStatus);

  return (
    <div className="mx-auto flex max-w-6xl flex-col gap-5">
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="Memories" value={total.toLocaleString()} />
        <Stat label="Active" value={(stats?.byStatus?.active ?? 0).toLocaleString()} tone="ok" />
        <Stat
          label="Awaiting review"
          value={(stats?.pendingCandidates ?? 0).toLocaleString()}
          tone={stats?.pendingCandidates ? 'warn' : undefined}
        />
        <Stat label="Relations" value={(stats?.edges ?? 0).toLocaleString()} hint="graph edges" />
      </div>

      <div className="grid gap-5 lg:grid-cols-2">
        <Panel title="By kind">
          <Bars data={stats?.byKind} onPick={() => go({ view: 'project', pid, tab: 'memories' })} />
        </Panel>
        <Panel title="By status">
          <Bars data={stats?.byStatus} />
        </Panel>
      </div>

      <div className="grid gap-5 lg:grid-cols-2">
        <Panel title="Store">
          <dl className="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-2 text-[13px]">
            <dt className="text-muted">Kind</dt>
            <dd>{ctx.project.storeKind === 'adopted' ? 'Adopted (in the project folder)' : 'Managed (in the data root)'}</dd>
            <dt className="text-muted">Access</dt>
            <dd>
              <Chip tone={ctx.project.admission}>{ctx.project.admission}</Chip>
            </dd>
            <dt className="text-muted">Store file</dt>
            <dd className="break-all font-mono text-xs">{ctx.project.storePath}</dd>
            <dt className="text-muted">Registered</dt>
            <dd title={absTime(ctx.project.createdAtMs)}>{relTime(ctx.project.createdAtMs)}</dd>
            <dt className="text-muted">Project id</dt>
            <dd className="font-mono text-xs">{pid}</dd>
          </dl>
        </Panel>
        <Panel title="Maintenance & recall">
          {sched.error ? (
            <ErrorNote error={sched.error} />
          ) : (
            <dl className="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-2 text-[13px]">
              <dt className="text-muted">Schedule</dt>
              <dd>{s ? (s.enabled ? `Every ${every(s.cadenceMs)}` : 'Disabled') : 'None'}</dd>
              <dt className="text-muted">Last run</dt>
              <dd title={absTime(s?.lastRunMs)}>
                {s?.lastRunMs ? relTime(s.lastRunMs) : '—'} {s?.lastOutcome && <StatusChip status={s.lastOutcome} />}
              </dd>
              <dt className="text-muted">Next run</dt>
              <dd title={absTime(s?.nextDueMs)}>{s?.nextDueMs ? relTime(s.nextDueMs) : '—'}</dd>
              <dt className="text-muted">Semantic recall</dt>
              <dd>
                {sem.data?.provider && sem.data.provider !== 'none'
                  ? `${sem.data.provider}${sem.data.indexed !== undefined ? ` · ${sem.data.indexed} indexed` : ''}`
                  : 'Words only (no semantic provider)'}
              </dd>
            </dl>
          )}
        </Panel>
      </div>
    </div>
  );
}
