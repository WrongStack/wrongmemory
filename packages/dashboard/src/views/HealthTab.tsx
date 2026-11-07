import { CircleAlert, CircleCheck, Info, Play, RefreshCw, TriangleAlert, Wrench } from 'lucide-react';
import { useState } from 'react';
import { change, errorText, read } from '../api';
import { absTime, duration, every, humanize, relTime } from '../format';
import type { DoctorCheck, DoctorReport, Job, Schedule } from '../types';
import { Button, Dialog, ErrorNote, Panel, Spinner, StatusChip, useInterval, useLoad, useToast } from '../ui';
import type { ProjectCtx } from './ProjectView';

const FIX_TEXT: Record<string, string> = {
  hygiene: 'Runs a hygiene pass now: expired memories are archived, stale anchors are marked, deleted rows past their retention are purged.',
  'graph-hubs': 'Trims same-topic / related links on nodes that collect too many of them, so the graph stays useful.',
  'dangling-edges': 'Removes links that point at memories which no longer exist.',
  'audience-runtime-mode': 'Moves runtime-mode audience entries into the field SAGE reads them from.',
  'fts-rebuild': 'Rebuilds the full-text search index from the stored memories.',
};

function SeverityIcon({ s }: { s: string }) {
  if (s === 'ok') return <CircleCheck className="size-4 text-ok" />;
  if (s === 'warn') return <TriangleAlert className="size-4 text-warn" />;
  if (s === 'error') return <CircleAlert className="size-4 text-bad" />;
  return <Info className="size-4 text-info" />;
}

const fixOf = (c: DoctorCheck) => c.fix ?? c.details?.fix;

export function HealthTab({ ctx }: { ctx: ProjectCtx }) {
  const pid = ctx.project.projectId;
  const toast = useToast();
  const writable = ctx.project.admission === 'read-write';
  const doctor = useLoad(() => read<DoctorReport>('memory.doctor', pid), [pid, ctx.version]);
  const maint = useLoad(() => read<{ schedule?: Schedule; recentJobs?: Job[] }>('maintenance.status', pid), [pid, ctx.version]);
  const [fixing, setFixing] = useState<DoctorCheck | null>(null);
  const [busy, setBusy] = useState(false);
  const [running, setRunning] = useState(false);

  const jobs = maint.data?.recentJobs ?? [];
  const inFlight = jobs.some((j) => j.state === 'queued' || j.state === 'running');
  useInterval(() => {
    maint.reload();
    if (!inFlight) doctor.reload();
  }, inFlight ? 2000 : null);

  async function applyFix(c: DoctorCheck) {
    const fix = fixOf(c);
    if (!fix) return;
    setBusy(true);
    try {
      await change('memory.doctor', pid, { fix: [fix] });
      toast('ok', `Applied “${humanize(fix)}”.`);
      setFixing(null);
      ctx.changed();
    } catch (e) {
      toast('bad', errorText(e));
    } finally {
      setBusy(false);
    }
  }

  async function runNow() {
    setRunning(true);
    try {
      const r = await change<{ coalesced?: boolean }>('maintenance.run', pid);
      toast('ok', r.coalesced ? 'A maintenance pass is already queued.' : 'Maintenance queued; it starts within a few seconds.');
      maint.reload();
    } catch (e) {
      toast('bad', errorText(e));
    } finally {
      setRunning(false);
    }
  }

  const s = maint.data?.schedule;
  const checks = [...(doctor.data?.checks ?? [])].sort((a, b) => {
    const order = ['error', 'warn', 'info', 'ok'];
    return order.indexOf(a.severity) - order.indexOf(b.severity);
  });

  return (
    <div className="mx-auto flex max-w-5xl flex-col gap-5">
      <Panel
        title="Maintenance"
        actions={
          writable && (
            <Button size="sm" tone="primary" busy={running} icon={<Play className="size-3.5" />} onClick={runNow}>
              Run maintenance now
            </Button>
          )
        }
      >
        {maint.error ? (
          <ErrorNote error={maint.error} />
        ) : !maint.data ? (
          <Spinner />
        ) : (
          <>
            <div className="grid gap-3 text-[13px] sm:grid-cols-3">
              <div>
                <div className="text-xs text-muted">Schedule</div>
                <div className="mt-0.5 font-medium">{s ? (s.enabled ? `Every ${every(s.cadenceMs)}` : 'Disabled') : 'None'}</div>
              </div>
              <div>
                <div className="text-xs text-muted">Last run</div>
                <div className="mt-0.5 font-medium" title={absTime(s?.lastRunMs)}>
                  {s?.lastRunMs ? relTime(s.lastRunMs) : '—'} {s?.lastOutcome && <StatusChip status={s.lastOutcome} />}
                </div>
              </div>
              <div>
                <div className="text-xs text-muted">Next run</div>
                <div className="mt-0.5 font-medium" title={absTime(s?.nextDueMs)}>
                  {s?.nextDueMs ? relTime(s.nextDueMs) : '—'}
                </div>
              </div>
            </div>
            {jobs.length > 0 && (
              <table className="mt-4 w-full text-[13px]">
                <thead className="text-left text-[11px] uppercase tracking-wide text-faint">
                  <tr>
                    <th className="py-1.5 font-medium">Job</th>
                    <th className="py-1.5 font-medium">State</th>
                    <th className="py-1.5 font-medium">Started</th>
                    <th className="py-1.5 font-medium">Took</th>
                  </tr>
                </thead>
                <tbody>
                  {jobs.slice(0, 8).map((j) => (
                    <tr key={j.jobId} className="border-t border-line">
                      <td className="py-1.5">{humanize(j.kind)}</td>
                      <td className="py-1.5">
                        <StatusChip status={j.state} />
                      </td>
                      <td className="py-1.5 text-muted" title={absTime(j.startedAtMs ?? j.createdAtMs)}>
                        {relTime(j.startedAtMs ?? j.createdAtMs)}
                      </td>
                      <td className="py-1.5 text-muted">
                        {j.finishedAtMs && (j.startedAtMs ?? j.createdAtMs) ? duration(j.finishedAtMs - (j.startedAtMs ?? j.createdAtMs ?? 0)) : '—'}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </>
        )}
      </Panel>

      <Panel
        title="Health check"
        actions={
          <Button size="sm" tone="ghost" icon={<RefreshCw className="size-3.5" />} onClick={doctor.reload} busy={doctor.loading}>
            Re-check
          </Button>
        }
      >
        {doctor.error ? (
          <ErrorNote error={doctor.error} />
        ) : !doctor.data ? (
          <Spinner label="Checking the store…" />
        ) : (
          <ul className="flex flex-col">
            {checks.map((c) => {
              const fix = fixOf(c);
              return (
                <li key={c.id} className="flex items-start gap-3 border-b border-line py-3 last:border-0">
                  <SeverityIcon s={c.severity} />
                  <div className="min-w-0 flex-1">
                    <div className="text-[13px] font-medium">{c.title ?? humanize(c.id)}</div>
                    {c.summary && <div className="mt-0.5 text-[13px] text-muted">{c.summary}</div>}
                  </div>
                  {fix && c.severity !== 'ok' && writable && (
                    <Button size="sm" icon={<Wrench className="size-3.5" />} onClick={() => setFixing(c)}>
                      Fix
                    </Button>
                  )}
                </li>
              );
            })}
          </ul>
        )}
        {doctor.data?.generatedAt && <p className="mt-3 text-xs text-faint">Checked {relTime(doctor.data.generatedAt)}.</p>}
      </Panel>

      {fixing && (
        <Dialog
          open
          title={`Fix: ${fixing.title ?? humanize(fixing.id)}`}
          onClose={() => setFixing(null)}
          footer={
            <>
              <Button onClick={() => setFixing(null)}>Cancel</Button>
              <Button tone="primary" busy={busy} icon={<Wrench className="size-4" />} onClick={() => applyFix(fixing)}>
                Apply
              </Button>
            </>
          }
        >
          <p className="text-[13px]">{fixing.summary}</p>
          <p className="mt-3 text-[13px] text-muted">{FIX_TEXT[fixOf(fixing) ?? ''] ?? 'Applies the named repair once.'}</p>
          <p className="mt-3 text-xs text-faint">The repair is recorded in the audit trail.</p>
        </Dialog>
      )}
    </div>
  );
}
