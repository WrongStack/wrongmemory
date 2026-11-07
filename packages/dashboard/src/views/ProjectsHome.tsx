import { Database, FolderGit2, Inbox, Trash2 } from 'lucide-react';
import { useMemo, useState } from 'react';
import { statsOf, useApp } from '../App';
import { relTime, sum } from '../format';
import type { Project } from '../types';
import { Button, Chip, cx, Empty, Spinner, Stat } from '../ui';
import { RemoveProjectsDialog } from './RemoveProjects';

export function ProjectsHome() {
  const { projects, stats, loaded, go } = useApp();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [removing, setRemoving] = useState<Project[] | null>(null);
  const [hideEmpty, setHideEmpty] = useState(false);

  const rows = useMemo(() => {
    const total = (p: Project) => sum(statsOf(stats[p.projectId])?.byStatus);
    return [...projects]
      .filter((p) => !hideEmpty || total(p) > 0)
      .sort((a, b) => total(b) - total(a) || (a.displayName ?? '').localeCompare(b.displayName ?? ''));
  }, [projects, stats, hideEmpty]);

  const totals = useMemo(() => {
    let memories = 0;
    let active = 0;
    let pending = 0;
    let empty = 0;
    for (const p of projects) {
      const st = statsOf(stats[p.projectId]);
      const n = sum(st?.byStatus);
      memories += n;
      active += st?.byStatus?.active ?? 0;
      pending += st?.pendingCandidates ?? 0;
      if (!n) empty += 1;
    }
    return { memories, active, pending, empty };
  }, [projects, stats]);

  const emptyIds = projects.filter((p) => !sum(statsOf(stats[p.projectId])?.byStatus)).map((p) => p.projectId);
  const toggle = (id: string) =>
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  if (!loaded) return <div className="p-8"><Spinner /></div>;

  return (
    <div className="mx-auto max-w-6xl p-6">
      <h1 className="text-xl font-semibold">Projects</h1>
      <p className="mt-1 text-[13px] text-muted">Every project this WrongMemory service keeps memory for.</p>

      <div className="mt-5 grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="Projects" value={projects.length} hint={totals.empty ? `${totals.empty} empty` : undefined} />
        <Stat label="Memories" value={totals.memories.toLocaleString()} />
        <Stat label="Active" value={totals.active.toLocaleString()} tone="ok" />
        <Stat label="Awaiting review" value={totals.pending.toLocaleString()} tone={totals.pending ? 'warn' : undefined} />
      </div>

      {projects.length === 0 ? (
        <div className="mt-6">
          <Empty icon={<Inbox className="size-8" />} title="No projects yet">
            WrongStack registers a project the first time it is used in <code className="font-mono">wrongmemory</code> mode, or run{' '}
            <code className="font-mono">wrongmemory project add &lt;root&gt;</code>.
          </Empty>
        </div>
      ) : (
        <>
          <div className="mt-6 flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-2 text-[13px] text-muted">
              <input type="checkbox" checked={hideEmpty} onChange={(e) => setHideEmpty(e.target.checked)} /> Hide empty projects
            </label>
            <div className="ml-auto flex items-center gap-2">
              {emptyIds.length > 0 && (
                <Button size="sm" onClick={() => setSelected(new Set(emptyIds))}>
                  Select {emptyIds.length} empty
                </Button>
              )}
              {selected.size > 0 && (
                <>
                  <Button size="sm" tone="ghost" onClick={() => setSelected(new Set())}>
                    Clear selection
                  </Button>
                  <Button
                    size="sm"
                    tone="danger"
                    icon={<Trash2 className="size-3.5" />}
                    onClick={() => setRemoving(projects.filter((p) => selected.has(p.projectId)))}
                  >
                    Remove {selected.size} selected
                  </Button>
                </>
              )}
            </div>
          </div>

          <div className="mt-3 overflow-hidden rounded-lg border border-line bg-panel">
            <table className="w-full text-[13px]">
              <thead className="border-b border-line bg-panel-2 text-left text-[11px] uppercase tracking-wide text-faint">
                <tr>
                  <th className="w-10 px-3 py-2" />
                  <th className="w-[44%] px-3 py-2 font-medium">Project</th>
                  <th className="px-3 py-2 font-medium">Store</th>
                  <th className="px-3 py-2 text-right font-medium">Active</th>
                  <th className="px-3 py-2 text-right font-medium">Total</th>
                  <th className="px-3 py-2 text-right font-medium">Review</th>
                  <th className="px-3 py-2 font-medium">Registered</th>
                  <th className="w-12 px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {rows.map((p) => {
                  const st = statsOf(stats[p.projectId]);
                  const n = sum(st?.byStatus);
                  return (
                    <tr
                      key={p.projectId}
                      className={cx('cursor-pointer border-b border-line last:border-0 hover:bg-panel-2', selected.has(p.projectId) && 'bg-accent-soft')}
                      onClick={() => go({ view: 'project', pid: p.projectId, tab: 'overview' })}
                    >
                      <td className="px-3 py-2.5" onClick={(e) => e.stopPropagation()}>
                        <input type="checkbox" aria-label="Select" checked={selected.has(p.projectId)} onChange={() => toggle(p.projectId)} />
                      </td>
                      <td className="max-w-0 px-3 py-2.5">
                        <div className="flex items-center gap-2">
                          {p.storeKind === 'adopted' ? (
                            <FolderGit2 className="size-4 shrink-0 text-accent" />
                          ) : (
                            <Database className="size-4 shrink-0 text-faint" />
                          )}
                          <span className="truncate font-medium">{p.displayName || p.projectId}</span>
                        </div>
                        <div className="truncate pl-6 font-mono text-[11px] text-faint" title={p.root}>
                          {p.root}
                        </div>
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex flex-wrap gap-1">
                          <Chip>{p.storeKind}</Chip>
                          <Chip tone={p.admission} title={p.admissionReason}>
                            {p.admission}
                          </Chip>
                        </div>
                      </td>
                      <td className="px-3 py-2.5 text-right tabular-nums">{(st?.byStatus?.active ?? 0).toLocaleString()}</td>
                      <td className={cx('px-3 py-2.5 text-right tabular-nums', !n && 'text-faint')}>{n.toLocaleString()}</td>
                      <td className="px-3 py-2.5 text-right tabular-nums">
                        {st?.pendingCandidates ? <span className="text-warn">{st.pendingCandidates.toLocaleString()}</span> : <span className="text-faint">0</span>}
                      </td>
                      <td className="px-3 py-2.5 text-muted">{relTime(p.createdAtMs)}</td>
                      <td className="px-3 py-2.5" onClick={(e) => e.stopPropagation()}>
                        <Button size="sm" tone="ghost" aria-label="Remove project" title="Remove project" onClick={() => setRemoving([p])} icon={<Trash2 className="size-3.5" />} />
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </>
      )}
      {removing && <RemoveProjectsDialog projects={removing} onClose={() => setRemoving(null)} onDone={() => setSelected(new Set())} />}
    </div>
  );
}
