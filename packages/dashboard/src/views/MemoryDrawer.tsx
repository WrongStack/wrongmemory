import {
  ArrowRight,
  FileCode,
  Folder,
  GitBranch,
  Hash,
  Network,
  Pencil,
  RotateCcw,
  Save,
  ShieldAlert,
  Terminal,
  Trash2,
  X,
} from 'lucide-react';
import { type ReactNode, useState } from 'react';
import { change, errorText, read } from '../api';
import { absTime, humanize, preview, relTime } from '../format';
import type { Anchor, Memory } from '../types';
import { Button, Chip, Dialog, ErrorNote, KindIcon, Meter, Spinner, StatusChip, useLoad, useToast } from '../ui';
import { anchorLabel } from './MemoriesTab';
import type { ProjectCtx } from './ProjectView';

export const KINDS = [
  'fact',
  'decision',
  'convention',
  'preference',
  'warning',
  'anti_pattern',
  'workflow',
  'bug_root_cause',
  'file_note',
  'symbol_note',
  'command_note',
  'summary',
  'memory_review',
  'tool_outcome',
  'error_pattern',
  'session_digest',
  'role_operational',
  'task_outcome',
  'security_signal',
  'fleet_convention',
];
const EDIT_STATUSES = ['active', 'stale', 'archived'];
const PERSISTENCE = ['permanent', 'long_lived', 'short_lived'];

function AnchorIcon({ a }: { a: Anchor }) {
  const cls = 'size-3.5 shrink-0 text-faint';
  if (a.type === 'directory') return <Folder className={cls} />;
  if (a.type === 'symbol') return <Hash className={cls} />;
  if (a.type === 'command') return <Terminal className={cls} />;
  return <FileCode className={cls} />;
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="border-t border-line px-5 py-4">
      <h3 className="mb-2.5 text-[11px] font-semibold uppercase tracking-wide text-faint">{title}</h3>
      {children}
    </section>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-muted">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </>
  );
}

export function MemoryDrawer({ ctx, id, onClose }: { ctx: ProjectCtx; id: string; onClose: () => void }) {
  const pid = ctx.project.projectId;
  const mem = useLoad(() => read<Memory>('memory.get', pid, { id }), [pid, id, ctx.version]);
  const related = useLoad(
    () => read<{ hits: { memory: Memory; reasons?: string[] }[] }>('memory.related', pid, { ids: [id], limit: 6 }),
    [pid, id, ctx.version],
  );
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const writable = ctx.project.admission === 'read-write';
  const m = mem.data;

  async function recover() {
    if (!m) return;
    setBusy(true);
    try {
      await change('memory.recover', pid, { id: m.id, reason: 'Recovered from the WrongMemory dashboard' });
      toast('ok', 'Memory recovered.');
      ctx.changed();
    } catch (e) {
      toast('bad', errorText(e));
    } finally {
      setBusy(false);
    }
  }

  const link = (target: string) => (
    <button
      key={target}
      type="button"
      onClick={() => ctx.openMemory(target)}
      className="flex items-center gap-1 font-mono text-xs text-accent hover:underline"
    >
      {target} <ArrowRight className="size-3" />
    </button>
  );

  return (
    <div className="fixed inset-0 z-40 flex justify-end bg-black/40" onMouseDown={onClose}>
      <aside
        className="flex h-full w-full max-w-2xl flex-col border-l border-line bg-panel shadow-2xl"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <header className="flex items-center gap-3 border-b border-line px-5 py-3">
          {m && <KindIcon kind={m.kind} />}
          <div className="min-w-0">
            <div className="text-[15px] font-semibold">{m ? humanize(m.kind) : 'Memory'}</div>
            <div className="font-mono text-[11px] text-faint">
              {id}
              {m && ` · revision ${m.revision}`}
            </div>
          </div>
          <div className="ml-auto flex items-center gap-1.5">
            {m && !editing && (
              <>
                <Button size="sm" icon={<Network className="size-3.5" />} onClick={() => ctx.showInGraph(`mem:${m.id}`)}>
                  Relations
                </Button>
                {writable && m.status !== 'deleted' && (
                  <>
                    <Button size="sm" icon={<Pencil className="size-3.5" />} onClick={() => setEditing(true)}>
                      Edit
                    </Button>
                    <Button size="sm" tone="danger" icon={<Trash2 className="size-3.5" />} onClick={() => setDeleting(true)}>
                      Delete
                    </Button>
                  </>
                )}
                {writable && m.status === 'deleted' && (
                  <Button size="sm" tone="ok" busy={busy} icon={<RotateCcw className="size-3.5" />} onClick={recover}>
                    Recover
                  </Button>
                )}
              </>
            )}
            <Button tone="ghost" size="sm" aria-label="Close" onClick={onClose} icon={<X className="size-4" />} />
          </div>
        </header>

        <div className="flex-1 overflow-y-auto">
          {mem.error ? (
            <div className="p-5">
              <ErrorNote error={mem.error} />
            </div>
          ) : !m ? (
            <div className="px-5">
              <Spinner />
            </div>
          ) : editing ? (
            <MemoryEditor
              ctx={ctx}
              m={m}
              onDone={() => {
                setEditing(false);
                ctx.changed();
              }}
              onCancel={() => setEditing(false)}
            />
          ) : (
            <>
              <div className="px-5 py-4">
                <div className="mb-3 flex flex-wrap items-center gap-1.5">
                  <StatusChip status={m.status} />
                  <Chip>{m.scope}</Chip>
                  {m.persistence && <Chip>{humanize(m.persistence)}</Chip>}
                  {m.contextPolicy && m.contextPolicy !== 'default' && <Chip tone="warn">inject: {m.contextPolicy}</Chip>}
                  {m.ownerSessionId && <Chip title={m.ownerSessionId}>session-private</Chip>}
                </div>
                <p className="whitespace-pre-wrap break-words text-[14.5px] leading-6">{m.text}</p>
                {m.staleReason && (
                  <div className="mt-3 rounded-md border border-warn/40 bg-warn-soft px-3 py-2 text-[13px] text-warn">Stale: {m.staleReason}</div>
                )}
                {m.validity?.statement && (
                  <div className="mt-3 flex gap-2 rounded-md border border-info/40 bg-info-soft px-3 py-2 text-[13px]">
                    <ShieldAlert className="mt-0.5 size-4 shrink-0 text-info" />
                    <div>
                      <div className="text-xs font-medium text-info">Holds while</div>
                      {m.validity.statement}
                    </div>
                  </div>
                )}
              </div>

              <Section title="Details">
                <dl className="grid grid-cols-[9rem_1fr] gap-x-4 gap-y-2 text-[13px]">
                  <Fact label="Importance">
                    <span className="flex items-center gap-2">
                      <Meter value={m.importance} /> {m.importance.toFixed(2)}
                    </span>
                  </Fact>
                  <Fact label="Confidence">
                    <span className="flex items-center gap-2">
                      <Meter value={m.confidence} tone="ok" /> {m.confidence.toFixed(2)}
                    </span>
                  </Fact>
                  <Fact label="Created">
                    <span title={absTime(m.createdAt)}>{relTime(m.createdAt)}</span>
                  </Fact>
                  <Fact label="Updated">
                    <span title={absTime(m.updatedAt)}>{relTime(m.updatedAt)}</span>
                  </Fact>
                  <Fact label="Used">
                    {m.useCount ?? 0}× used, {m.injectionCount ?? 0}× injected
                    {m.lastUsedAt && <span className="text-muted"> · last {relTime(m.lastUsedAt)}</span>}
                  </Fact>
                  {m.lastVerifiedAt && <Fact label="Verified">{relTime(m.lastVerifiedAt)}</Fact>}
                  {m.expiresAt && <Fact label="Expires">{relTime(m.expiresAt)}</Fact>}
                  {m.wrongmemory?.client && <Fact label="Written by">{m.wrongmemory.client}</Fact>}
                </dl>
              </Section>

              {(m.anchors ?? []).length > 0 && (
                <Section title="Anchored to">
                  <ul className="flex flex-col gap-1">
                    {m.anchors.map((a, i) => (
                      <li key={i} className="flex items-center gap-2 text-[13px]">
                        <AnchorIcon a={a} />
                        <button
                          type="button"
                          className="min-w-0 truncate font-mono text-xs hover:text-accent hover:underline"
                          title="Show everything attached to this path"
                          onClick={() => ctx.showInGraph(a.path ?? anchorLabel(a))}
                        >
                          {anchorLabel(a)}
                        </button>
                        <span className="text-[11px] text-faint">{a.type}</span>
                      </li>
                    ))}
                  </ul>
                </Section>
              )}

              {(m.tags ?? []).length > 0 && (
                <Section title="Tags">
                  <div className="flex flex-wrap gap-1.5">
                    {m.tags.map((t) => (
                      <Chip key={t}>#{t}</Chip>
                    ))}
                  </div>
                </Section>
              )}

              {(m.supersedes?.length || m.contradicts?.length || m.supersededBy) && (
                <Section title="Lineage">
                  <div className="flex flex-col gap-2 text-[13px]">
                    {m.supersededBy && (
                      <div className="flex flex-wrap items-center gap-2">
                        <GitBranch className="size-3.5 text-warn" /> Replaced by {link(m.supersededBy)}
                      </div>
                    )}
                    {!!m.supersedes?.length && (
                      <div className="flex flex-wrap items-center gap-2">
                        <GitBranch className="size-3.5 text-accent" /> Replaces {m.supersedes.map(link)}
                      </div>
                    )}
                    {!!m.contradicts?.length && (
                      <div className="flex flex-wrap items-center gap-2">
                        <ShieldAlert className="size-3.5 text-bad" /> Contradicts {m.contradicts.map(link)}
                      </div>
                    )}
                  </div>
                </Section>
              )}

              <Section title="Related memories">
                {related.loading && !related.data ? (
                  <Spinner />
                ) : !related.data?.hits.length ? (
                  <p className="text-[13px] text-muted">Nothing related through files, tags or links.</p>
                ) : (
                  <ul className="flex flex-col gap-1.5">
                    {related.data.hits.map((h) => (
                      <li key={h.memory.id}>
                        <button
                          type="button"
                          onClick={() => ctx.openMemory(h.memory.id)}
                          className="flex w-full items-start gap-2.5 rounded-md border border-line px-3 py-2 text-left hover:bg-panel-2"
                        >
                          <KindIcon kind={h.memory.kind} size="sm" />
                          <div className="min-w-0">
                            <div className="line-clamp-2 text-[13px]">{preview(h.memory.text, 200)}</div>
                            {!!h.reasons?.length && <div className="mt-0.5 truncate text-[11px] text-faint">{h.reasons.join(' · ')}</div>}
                          </div>
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </Section>

              {(m.sources ?? []).length > 0 && (
                <Section title="Where it came from">
                  <ul className="flex flex-col gap-1 text-[13px]">
                    {m.sources.map((s, i) => (
                      <li key={i} className="flex flex-wrap items-center gap-2">
                        <Chip>{humanize(s.type)}</Chip>
                        {s.path && <span className="font-mono text-xs text-muted">{s.path}</span>}
                        {s.command && <span className="font-mono text-xs text-muted">{s.command}</span>}
                        {s.sessionId && <span className="font-mono text-[11px] text-faint">session {s.sessionId}</span>}
                      </li>
                    ))}
                  </ul>
                </Section>
              )}

              {(m.feedback ?? []).length > 0 && (
                <Section title="Feedback">
                  <ul className="flex flex-col gap-1.5 text-[13px]">
                    {(m.feedback ?? []).map((f, i) => (
                      <li key={i} className="flex flex-wrap items-center gap-2">
                        {f.verdict && <Chip tone={f.verdict === 'helpful' || f.verdict === 'confirmed' ? 'ok' : 'warn'}>{String(f.verdict)}</Chip>}
                        <span>{f.note ? String(f.note) : ''}</span>
                        {f.at && <span className="text-xs text-faint">{relTime(String(f.at))}</span>}
                      </li>
                    ))}
                  </ul>
                </Section>
              )}

              <Section title="Raw record">
                <details>
                  <summary className="cursor-pointer text-[13px] text-muted hover:text-fg">Show JSON</summary>
                  <pre className="mt-2 max-h-96 overflow-auto rounded-md border border-line bg-panel-2 p-3 font-mono text-[11.5px] leading-5">
                    {JSON.stringify(m, null, 2)}
                  </pre>
                </details>
              </Section>
            </>
          )}
        </div>
      </aside>
      {deleting && m && (
        <DeleteDialog
          ctx={ctx}
          m={m}
          onClose={() => setDeleting(false)}
          onDone={() => {
            setDeleting(false);
            ctx.changed();
          }}
        />
      )}
    </div>
  );
}

function MemoryEditor({ ctx, m, onDone, onCancel }: { ctx: ProjectCtx; m: Memory; onDone: () => void; onCancel: () => void }) {
  const toast = useToast();
  const [text, setText] = useState(m.text);
  const [kind, setKind] = useState(m.kind);
  const [status, setStatus] = useState(m.status);
  const [importance, setImportance] = useState(m.importance);
  const [confidence, setConfidence] = useState(m.confidence);
  const [persistence, setPersistence] = useState(m.persistence ?? '');
  const [tags, setTags] = useState((m.tags ?? []).join(', '));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();

  const statuses = EDIT_STATUSES.includes(m.status) ? EDIT_STATUSES : [m.status, ...EDIT_STATUSES];
  const kinds = KINDS.includes(m.kind) ? KINDS : [m.kind, ...KINDS];

  async function save() {
    const patch: Record<string, unknown> = { id: m.id, expectedRevision: m.revision };
    if (text.trim() !== m.text) patch.text = text.trim();
    if (kind !== m.kind) patch.kind = kind;
    if (status !== m.status) patch.status = status;
    if (importance !== m.importance) patch.importance = importance;
    if (confidence !== m.confidence) patch.confidence = confidence;
    if (persistence && persistence !== m.persistence) patch.persistence = persistence;
    const nextTags = tags
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean);
    if (nextTags.join('\u0000') !== (m.tags ?? []).join('\u0000')) patch.tags = nextTags;
    if (Object.keys(patch).length === 2) return onCancel();
    setBusy(true);
    try {
      await change('memory.update', ctx.project.projectId, patch);
      toast('ok', 'Memory saved.');
      onDone();
    } catch (e) {
      setError(e);
      if (errorText(e).startsWith('REVISION_CONFLICT')) toast('bad', 'Someone changed this memory meanwhile. Reopen it to see the new revision.');
    } finally {
      setBusy(false);
    }
  }

  const field = 'h-8 w-full rounded-md border border-line bg-panel-2 px-2.5 text-[13px] outline-none focus:border-accent';
  return (
    <div className="flex flex-col gap-4 px-5 py-4">
      {error !== undefined && <ErrorNote error={error} />}
      <label className="block">
        <span className="mb-1 block text-xs font-medium text-muted">Text</span>
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          rows={8}
          className="w-full rounded-md border border-line bg-panel-2 px-3 py-2 text-[13.5px] leading-6 outline-none focus:border-accent"
        />
      </label>
      <div className="grid grid-cols-2 gap-3">
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-muted">Kind</span>
          <select value={kind} onChange={(e) => setKind(e.target.value)} className={field}>
            {kinds.map((k) => (
              <option key={k} value={k}>
                {humanize(k)}
              </option>
            ))}
          </select>
        </label>
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-muted">Status</span>
          <select value={status} onChange={(e) => setStatus(e.target.value)} className={field}>
            {statuses.map((s) => (
              <option key={s} value={s}>
                {humanize(s)}
              </option>
            ))}
          </select>
        </label>
        <label className="block">
          <span className="mb-1 flex justify-between text-xs font-medium text-muted">
            Importance <span className="tabular-nums text-fg">{importance.toFixed(2)}</span>
          </span>
          <input type="range" min={0} max={1} step={0.05} value={importance} onChange={(e) => setImportance(Number(e.target.value))} className="w-full accent-[var(--accent)]" />
        </label>
        <label className="block">
          <span className="mb-1 flex justify-between text-xs font-medium text-muted">
            Confidence <span className="tabular-nums text-fg">{confidence.toFixed(2)}</span>
          </span>
          <input type="range" min={0} max={1} step={0.05} value={confidence} onChange={(e) => setConfidence(Number(e.target.value))} className="w-full accent-[var(--accent)]" />
        </label>
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-muted">Persistence</span>
          <select value={persistence} onChange={(e) => setPersistence(e.target.value)} className={field}>
            {!m.persistence && <option value="">Default</option>}
            {PERSISTENCE.map((p) => (
              <option key={p} value={p}>
                {humanize(p)}
              </option>
            ))}
          </select>
        </label>
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-muted">Tags (comma separated)</span>
          <input value={tags} onChange={(e) => setTags(e.target.value)} className={field} />
        </label>
      </div>
      <p className="text-xs text-faint">
        Saving creates revision {m.revision + 1}. If an agent changed this memory since you opened it, the save is refused instead of overwriting their change.
      </p>
      <div className="flex justify-end gap-2">
        <Button onClick={onCancel}>Cancel</Button>
        <Button tone="primary" busy={busy} icon={<Save className="size-4" />} onClick={save}>
          Save
        </Button>
      </div>
    </div>
  );
}

function DeleteDialog({ ctx, m, onClose, onDone }: { ctx: ProjectCtx; m: Memory; onClose: () => void; onDone: () => void }) {
  const toast = useToast();
  const [reason, setReason] = useState('');
  const [never, setNever] = useState(false);
  const [overridePermanent, setOverridePermanent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const permanent = m.persistence === 'permanent';
  async function run() {
    setBusy(true);
    try {
      // SAGE requires `force` on every delete; this dialog is the confirmation.
      await change('memory.delete', ctx.project.projectId, {
        id: m.id,
        reason: reason.trim() || 'Deleted from the WrongMemory dashboard',
        neverInject: never,
        force: true,
      });
      toast('ok', 'Memory deleted. It can be recovered from the Deleted filter.');
      onDone();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  }
  return (
    <Dialog
      open
      title="Delete memory"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button tone="danger" busy={busy} disabled={permanent && !overridePermanent} icon={<Trash2 className="size-4" />} onClick={run}>
            Delete
          </Button>
        </>
      }
    >
      <p className="rounded-md border border-line bg-panel-2 px-3 py-2 text-[13px]">{preview(m.text, 260)}</p>
      <p className="mt-3 text-[13px] text-muted">
        The memory is kept as deleted (a tombstone): agents stop seeing it, its links are removed, and you can recover it later. The deletion is
        recorded in the audit trail.
      </p>
      {permanent && (
        <label className="mt-3 flex items-start gap-2 rounded-md border border-warn/40 bg-warn-soft px-3 py-2 text-[13px] text-warn">
          <input type="checkbox" className="mt-0.5" checked={overridePermanent} onChange={(e) => setOverridePermanent(e.target.checked)} />
          This memory is marked permanent. Delete it anyway (the override is audited).
        </label>
      )}
      <label className="mt-3 block">
        <span className="mb-1 block text-xs font-medium text-muted">Reason (recorded in the audit trail)</span>
        <input
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          placeholder="Outdated, wrong, duplicate…"
          className="h-8 w-full rounded-md border border-line bg-panel-2 px-2.5 text-[13px] outline-none focus:border-accent"
        />
      </label>
      <label className="mt-3 flex items-center gap-2 text-[13px]">
        <input type="checkbox" checked={never} onChange={(e) => setNever(e.target.checked)} /> Keep it out of agent context even if it is recovered
        later
      </label>
      {error !== undefined && (
        <div className="mt-3">
          <ErrorNote error={error} />
        </div>
      )}
    </Dialog>
  );
}
