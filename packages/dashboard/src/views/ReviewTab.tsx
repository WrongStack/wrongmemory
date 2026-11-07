import { ArrowRight, Check, FileCode, GitBranch, Inbox, X } from 'lucide-react';
import { useState } from 'react';
import { change, errorText, read } from '../api';
import { absTime, humanize, preview, relTime } from '../format';
import type { Candidate, Memory } from '../types';
import { Button, Chip, cx, Empty, ErrorNote, KindIcon, Spinner, StatusChip, useLoad, useToast } from '../ui';
import { anchorLabel } from './MemoriesTab';
import type { ProjectCtx } from './ProjectView';

const PAGE = 100;

function Target({ ctx, id }: { ctx: ProjectCtx; id: string }) {
  const t = useLoad(() => read<Memory>('memory.get', ctx.project.projectId, { id }), [id]);
  return (
    <div className="mt-2 rounded-md border border-line bg-panel-2 px-3 py-2 text-[13px]">
      <div className="mb-1 flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-faint">
        Current memory
        <button type="button" onClick={() => ctx.openMemory(id)} className="ml-auto flex items-center gap-1 font-mono normal-case tracking-normal text-accent hover:underline">
          {id} <ArrowRight className="size-3" />
        </button>
      </div>
      {t.error ? <span className="text-muted">Not available ({errorText(t.error)})</span> : t.data ? <p className="text-muted">{preview(t.data.text, 400)}</p> : '…'}
    </div>
  );
}

function CandidateCard({
  ctx,
  c,
  selected,
  onSelect,
  onDecided,
  pending,
}: {
  ctx: ProjectCtx;
  c: Candidate;
  selected: boolean;
  onSelect: () => void;
  onDecided: () => void;
  pending: boolean;
}) {
  const toast = useToast();
  const [busy, setBusy] = useState<'accept' | 'reject' | null>(null);
  const [rejecting, setRejecting] = useState(false);
  const [reason, setReason] = useState('');
  const writable = ctx.project.admission === 'read-write';

  async function decide(kind: 'accept' | 'reject') {
    setBusy(kind);
    try {
      const r = await change<{ applied?: boolean; error?: unknown }>(
        kind === 'accept' ? 'candidate.accept' : 'candidate.reject',
        ctx.project.projectId,
        kind === 'accept' ? { id: c.id } : { id: c.id, reason: reason.trim() || 'Rejected in the WrongMemory dashboard' },
      );
      if (r && r.applied === false) toast('bad', `Not applied: ${errorText(r.error)}`);
      else toast('ok', kind === 'accept' ? 'Accepted.' : 'Rejected.');
      onDecided();
    } catch (e) {
      toast('bad', errorText(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className={cx('rounded-lg border bg-panel px-4 py-3', selected ? 'border-accent' : 'border-line')}>
      <div className="flex items-start gap-3">
        {pending && writable && <input type="checkbox" aria-label="Select" checked={selected} onChange={onSelect} className="mt-2" />}
        <KindIcon kind={c.kind} />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-1.5 text-xs text-muted">
            <span className="font-medium text-fg/80">{humanize(c.kind)}</span>
            {c.suggestedAction && <Chip tone="pending">{humanize(c.suggestedAction)}</Chip>}
            {!pending && <StatusChip status={c.status} />}
            <span title={absTime(c.createdAt)}>· proposed {relTime(c.createdAt)}</span>
          </div>
          <p className="mt-1.5 whitespace-pre-wrap break-words text-[13.5px] leading-6">{c.text}</p>
          {(c.reason || c.reviewReason) && <p className="mt-1.5 text-[13px] text-muted">Why: {c.reviewReason ?? c.reason}</p>}
          {(c.anchors ?? []).length > 0 && (
            <div className="mt-2 flex flex-wrap gap-1.5">
              {c.anchors.slice(0, 4).map((a, i) => (
                <Chip key={i} className="max-w-72 font-mono">
                  <FileCode className="size-3 shrink-0" />
                  <span className="truncate">{anchorLabel(a)}</span>
                </Chip>
              ))}
            </div>
          )}
          {c.targetMemoryId && <Target ctx={ctx} id={c.targetMemoryId} />}
          {!!c.supersedesMemoryIds?.length && (
            <div className="mt-2 flex flex-wrap items-center gap-2 text-[13px] text-muted">
              <GitBranch className="size-3.5 text-warn" /> Would replace
              {c.supersedesMemoryIds.map((id) => (
                <button key={id} type="button" className="font-mono text-xs text-accent hover:underline" onClick={() => ctx.openMemory(id)}>
                  {id}
                </button>
              ))}
            </div>
          )}
          {c.memoryId && !pending && (
            <button type="button" className="mt-2 flex items-center gap-1 font-mono text-xs text-accent hover:underline" onClick={() => ctx.openMemory(c.memoryId as string)}>
              Became {c.memoryId} <ArrowRight className="size-3" />
            </button>
          )}
          {rejecting && (
            <div className="mt-3 flex gap-2">
              <input
                autoFocus
                value={reason}
                onChange={(e) => setReason(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && decide('reject')}
                placeholder="Reason (optional): duplicate, wrong, too specific…"
                className="h-8 flex-1 rounded-md border border-line bg-panel-2 px-2.5 text-[13px] outline-none focus:border-accent"
              />
              <Button tone="danger" busy={busy === 'reject'} onClick={() => decide('reject')}>
                Reject
              </Button>
              <Button tone="ghost" onClick={() => setRejecting(false)}>
                Cancel
              </Button>
            </div>
          )}
        </div>
        {pending && writable && !rejecting && (
          <div className="flex shrink-0 gap-1.5">
            <Button size="sm" tone="ok" busy={busy === 'accept'} icon={<Check className="size-3.5" />} onClick={() => decide('accept')}>
              Accept
            </Button>
            <Button size="sm" icon={<X className="size-3.5" />} onClick={() => setRejecting(true)}>
              Reject
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}

export function ReviewTab({ ctx }: { ctx: ProjectCtx }) {
  const pid = ctx.project.projectId;
  const toast = useToast();
  const [status, setStatus] = useState('pending');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [batch, setBatch] = useState<{ done: number; total: number } | null>(null);
  const list = useLoad(
    () => read<{ candidates: Candidate[]; truncated?: boolean }>('candidate.list', pid, { status, limit: PAGE }),
    [pid, status, ctx.version],
  );
  const items = list.data?.candidates ?? [];
  const pending = status === 'pending';

  async function runBatch(kind: 'accept' | 'reject') {
    const ids = items.filter((c) => selected.has(c.id)).map((c) => c.id);
    setBatch({ done: 0, total: ids.length });
    let failed = 0;
    for (const [i, id] of ids.entries()) {
      try {
        const r = await change<{ applied?: boolean }>(
          kind === 'accept' ? 'candidate.accept' : 'candidate.reject',
          pid,
          kind === 'accept' ? { id } : { id, reason: 'Rejected in the WrongMemory dashboard' },
        );
        if (r && r.applied === false) failed += 1;
      } catch {
        failed += 1;
      }
      setBatch({ done: i + 1, total: ids.length });
    }
    setBatch(null);
    setSelected(new Set());
    toast(failed ? 'bad' : 'ok', `${kind === 'accept' ? 'Accepted' : 'Rejected'} ${ids.length - failed} of ${ids.length}.`);
    ctx.changed();
  }

  return (
    <div className="mx-auto flex max-w-5xl flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex rounded-md border border-line bg-panel p-0.5">
          {['pending', 'accepted', 'rejected', 'merged'].map((s) => (
            <button
              key={s}
              type="button"
              onClick={() => {
                setStatus(s);
                setSelected(new Set());
              }}
              className={cx('rounded px-3 py-1 text-[13px]', status === s ? 'bg-raised text-fg' : 'text-muted hover:text-fg')}
            >
              {humanize(s)}
            </button>
          ))}
        </div>
        {pending && items.length > 0 && ctx.project.admission === 'read-write' && (
          <div className="ml-auto flex items-center gap-2">
            <Button size="sm" tone="ghost" onClick={() => setSelected(selected.size === items.length ? new Set() : new Set(items.map((c) => c.id)))}>
              {selected.size === items.length ? 'Select none' : `Select all ${items.length}`}
            </Button>
            {selected.size > 0 && (
              <>
                <Button size="sm" tone="ok" busy={!!batch} icon={<Check className="size-3.5" />} onClick={() => runBatch('accept')}>
                  Accept {selected.size}
                </Button>
                <Button size="sm" tone="danger" busy={!!batch} icon={<X className="size-3.5" />} onClick={() => runBatch('reject')}>
                  Reject {selected.size}
                </Button>
              </>
            )}
          </div>
        )}
      </div>
      {batch && (
        <div className="text-[13px] text-muted">
          Working… {batch.done} / {batch.total}
        </div>
      )}
      {pending && (
        <p className="text-[13px] text-muted">
          Agents propose memories and changes here instead of writing them directly. Accepting writes the memory (or applies the change); rejecting
          keeps it out.
          {list.data?.truncated && ` Showing ${PAGE} at a time; more appear as you decide.`}
        </p>
      )}
      {list.error ? (
        <ErrorNote error={list.error} />
      ) : list.loading && !list.data ? (
        <Spinner />
      ) : !items.length ? (
        <Empty icon={<Inbox className="size-8" />} title={pending ? 'Nothing to review' : `No ${status} proposals`}>
          {pending ? 'New proposals from agents will show up here.' : undefined}
        </Empty>
      ) : (
        <div className="flex flex-col gap-2">
          {items.map((c) => (
            <CandidateCard
              key={c.id}
              ctx={ctx}
              c={c}
              pending={pending}
              selected={selected.has(c.id)}
              onSelect={() =>
                setSelected((s) => {
                  const n = new Set(s);
                  if (n.has(c.id)) n.delete(c.id);
                  else n.add(c.id);
                  return n;
                })
              }
              onDecided={ctx.changed}
            />
          ))}
        </div>
      )}
    </div>
  );
}
