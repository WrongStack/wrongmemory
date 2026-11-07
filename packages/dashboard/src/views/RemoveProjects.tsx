import { Trash2 } from 'lucide-react';
import { useState } from 'react';
import { change, errorText } from '../api';
import { statsOf, useApp } from '../App';
import { sum } from '../format';
import type { Project } from '../types';
import { Button, Chip, Dialog, useToast } from '../ui';

/**
 * Removes projects from WrongMemory (`project.detach`). Store files stay where
 * they are; an adopted store goes back to local SAGE.
 */
export function RemoveProjectsDialog({
  projects,
  onClose,
  onDone,
}: {
  projects: Project[];
  onClose: () => void;
  onDone?: () => void;
}) {
  const { stats, refreshProjects } = useApp();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [typed, setTyped] = useState('');
  const withMemories = projects.filter((p) => sum(statsOf(stats[p.projectId])?.byStatus) > 0);
  const needsConfirm = withMemories.length > 0;
  const ready = !needsConfirm || typed.trim().toLowerCase() === 'remove';

  async function run() {
    setBusy(true);
    let ok = 0;
    for (const p of projects) {
      try {
        await change('project.detach', p.projectId);
        ok += 1;
      } catch (e) {
        toast('bad', `${p.displayName || p.projectId}: ${errorText(e)}`);
      }
    }
    await refreshProjects().catch(() => {});
    setBusy(false);
    if (ok) toast('ok', ok === 1 ? `Removed ${projects[0].displayName || projects[0].projectId}.` : `Removed ${ok} projects.`);
    onDone?.();
    onClose();
  }

  return (
    <Dialog
      open
      title={projects.length === 1 ? 'Remove project' : `Remove ${projects.length} projects`}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button tone="danger" icon={<Trash2 className="size-4" />} busy={busy} disabled={!ready} onClick={run}>
            Remove
          </Button>
        </>
      }
    >
      <ul className="mb-4 flex max-h-48 flex-col gap-1.5 overflow-y-auto">
        {projects.map((p) => {
          const n = sum(statsOf(stats[p.projectId])?.byStatus);
          return (
            <li key={p.projectId} className="flex items-center gap-2 rounded-md border border-line bg-panel-2 px-3 py-2">
              <div className="min-w-0 flex-1">
                <div className="truncate font-medium">{p.displayName || p.projectId}</div>
                <div className="truncate font-mono text-[11px] text-faint">{p.root}</div>
              </div>
              <Chip>{p.storeKind}</Chip>
              <Chip tone={n ? 'warn' : undefined}>{n} memories</Chip>
            </li>
          );
        })}
      </ul>
      <div className="space-y-2 text-[13px] text-muted">
        <p>WrongMemory stops serving {projects.length === 1 ? 'this project' : 'these projects'}. Nothing is deleted:</p>
        <ul className="list-disc space-y-1 pl-5">
          <li>
            <strong className="text-fg">Adopted</strong> stores stay in the project folder (<code className="font-mono">.wrongstack/memories/sage.db</code>) and
            go back to local SAGE.
          </li>
          <li>
            <strong className="text-fg">Managed</strong> stores stay under the data root (<code className="font-mono">~/.wrongmemory/projects</code>).
          </li>
          <li>A WrongStack host in <code className="font-mono">wrongmemory</code> mode registers a project again the next time it is used in that folder.</li>
        </ul>
      </div>
      {needsConfirm && (
        <label className="mt-4 block text-[13px]">
          <span className="text-muted">
            {withMemories.length === 1 ? 'This project holds memories.' : `${withMemories.length} of these projects hold memories.`} Type{' '}
            <strong className="text-fg">remove</strong> to confirm.
          </span>
          <input
            autoFocus
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            className="mt-1.5 h-8 w-full rounded-md border border-line bg-panel-2 px-2.5 outline-none focus:border-accent"
          />
        </label>
      )}
    </Dialog>
  );
}
