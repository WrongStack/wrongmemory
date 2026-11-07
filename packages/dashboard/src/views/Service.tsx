import { KeyRound } from 'lucide-react';
import { read } from '../api';
import { useApp } from '../App';
import { absTime, duration, relTime } from '../format';
import type { Grant } from '../types';
import { Chip, Empty, ErrorNote, Panel, Spinner, Stat, useLoad } from '../ui';

export function GrantsView() {
  const { projects } = useApp();
  const grants = useLoad(() => read<{ grants: Grant[] }>('grant.list'), []);
  const name = (id: string) => projects.find((p) => p.projectId === id)?.displayName || id;
  const list = grants.data?.grants ?? [];
  return (
    <div className="mx-auto max-w-5xl p-6">
      <h1 className="text-xl font-semibold">Client grants</h1>
      <p className="mt-1 text-[13px] text-muted">Credentials issued to MCP clients (Claude Code, Codex, …). Secrets are never shown here.</p>
      <div className="mt-5">
        {grants.error ? (
          <ErrorNote error={grants.error} />
        ) : !grants.data ? (
          <Spinner />
        ) : !list.length ? (
          <Empty icon={<KeyRound className="size-8" />} title="No grants issued">
            Connect a client with <code className="font-mono">wrongmemory connect &lt;client&gt;</code>.
          </Empty>
        ) : (
          <div className="overflow-hidden rounded-lg border border-line bg-panel">
            <table className="w-full text-[13px]">
              <thead className="border-b border-line bg-panel-2 text-left text-[11px] uppercase tracking-wide text-faint">
                <tr>
                  <th className="px-3 py-2 font-medium">Label</th>
                  <th className="px-3 py-2 font-medium">Profile</th>
                  <th className="px-3 py-2 font-medium">Projects</th>
                  <th className="px-3 py-2 font-medium">Created</th>
                  <th className="px-3 py-2 font-medium">State</th>
                </tr>
              </thead>
              <tbody>
                {list.map((g) => {
                  const state = g.revokedAtMs ? 'revoked' : g.expiresAtMs && g.expiresAtMs < Date.now() ? 'expired' : 'active';
                  return (
                    <tr key={g.grantId} className="border-b border-line last:border-0">
                      <td className="px-3 py-2.5">
                        <div className="font-medium">{g.label}</div>
                        <div className="font-mono text-[11px] text-faint">{g.grantId}</div>
                      </td>
                      <td className="px-3 py-2.5">
                        <Chip>{g.profile}</Chip>
                      </td>
                      <td className="px-3 py-2.5 text-muted">{g.projects.map(name).join(', ')}</td>
                      <td className="px-3 py-2.5 text-muted" title={absTime(g.createdAtMs)}>
                        {relTime(g.createdAtMs)}
                      </td>
                      <td className="px-3 py-2.5">
                        <Chip tone={state === 'active' ? 'ok' : state === 'revoked' ? 'revoked' : 'warn'}>{state}</Chip>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

export function DaemonView() {
  const { health, healthError } = useApp();
  return (
    <div className="mx-auto max-w-5xl p-6">
      <h1 className="text-xl font-semibold">Daemon</h1>
      <p className="mt-1 text-[13px] text-muted">The background service that owns every project store. Closing this page never stops it.</p>
      {healthError !== undefined && !health && (
        <div className="mt-5">
          <ErrorNote error={healthError} />
        </div>
      )}
      {health && (
        <>
          <div className="mt-5 grid grid-cols-2 gap-3 md:grid-cols-4">
            <Stat label="Version" value={`v${health.daemonVersion}`} />
            <Stat label="Uptime" value={duration(health.uptimeMs)} />
            <Stat label="Open stores" value={health.openStores} hint={`${health.projects} registered`} />
            <Stat label="Connections" value={health.connections} />
          </div>
          <Panel title="Details" className="mt-5">
            <dl className="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-2 text-[13px]">
              <dt className="text-muted">Instance</dt>
              <dd className="font-mono text-xs">{health.daemonInstanceId}</dd>
              <dt className="text-muted">Process id</dt>
              <dd>{health.pid}</dd>
              <dt className="text-muted">Data root</dt>
              <dd className="font-mono text-xs">{health.dataRoot}</dd>
              <dt className="text-muted">SQLite</dt>
              <dd>{health.sqliteVersion}</dd>
              <dt className="text-muted">Draining</dt>
              <dd>{health.draining ? 'yes' : 'no'}</dd>
            </dl>
          </Panel>
        </>
      )}
    </div>
  );
}
