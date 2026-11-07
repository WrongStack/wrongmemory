import { Database, House, KeyRound, Server } from 'lucide-react';
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import { read } from './api';
import { duration, sum } from './format';
import type { Health, Project, Stats } from './types';
import { cx, useInterval } from './ui';
import { DaemonView, GrantsView } from './views/Service';
import { ProjectsHome } from './views/ProjectsHome';
import { ProjectView, type ProjectTab } from './views/ProjectView';

export type Route =
  | { view: 'home' }
  | { view: 'project'; pid: string; tab: ProjectTab; focus?: string }
  | { view: 'grants' }
  | { view: 'daemon' };

interface AppState {
  projects: Project[];
  stats: Record<string, Stats | { error: unknown }>;
  health?: Health;
  healthError?: unknown;
  loaded: boolean;
  refreshProjects: () => Promise<void>;
  go: (r: Route) => void;
  route: Route;
}

const Ctx = createContext<AppState | null>(null);

export function useApp(): AppState {
  const v = useContext(Ctx);
  if (!v) throw new Error('outside App');
  return v;
}

export const statsOf = (s: AppState['stats'][string] | undefined): Stats | undefined =>
  s && !('error' in s) ? s : undefined;

export function App() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [stats, setStats] = useState<AppState['stats']>({});
  const [health, setHealth] = useState<Health>();
  const [healthError, setHealthError] = useState<unknown>();
  const [loaded, setLoaded] = useState(false);
  const [route, setRoute] = useState<Route>({ view: 'home' });

  const refreshHealth = useCallback(() => {
    read<Health>('daemon.health')
      .then((h) => {
        setHealth(h);
        setHealthError(undefined);
      })
      .catch(setHealthError);
  }, []);

  const refreshProjects = useCallback(async () => {
    const r = await read<{ projects: Project[] }>('project.list');
    const list = r.projects ?? [];
    const entries = await Promise.all(
      list.map(async (p) => {
        try {
          return [p.projectId, await read<Stats>('memory.stats', p.projectId)] as const;
        } catch (error) {
          return [p.projectId, { error }] as const;
        }
      }),
    );
    setProjects(list);
    setStats(Object.fromEntries(entries));
    setLoaded(true);
  }, []);

  useEffect(() => {
    refreshHealth();
    refreshProjects().catch(() => setLoaded(true));
  }, [refreshHealth, refreshProjects]);
  useInterval(refreshHealth, 5000);
  useInterval(() => void refreshProjects().catch(() => {}), 20000);

  // A removed project's view falls back to the overview.
  useEffect(() => {
    if (route.view === 'project' && loaded && !projects.some((p) => p.projectId === route.pid)) {
      setRoute({ view: 'home' });
    }
  }, [route, projects, loaded]);

  const value = useMemo<AppState>(
    () => ({ projects, stats, health, healthError, loaded, refreshProjects, go: setRoute, route }),
    [projects, stats, health, healthError, loaded, refreshProjects, route],
  );

  return (
    <Ctx.Provider value={value}>
      <div className="flex h-full flex-col">
        <TopBar />
        <div className="flex min-h-0 flex-1">
          <Sidebar />
          <main className="min-w-0 flex-1 overflow-y-auto">
            {route.view === 'home' && <ProjectsHome />}
            {route.view === 'project' && <ProjectView key={route.pid} pid={route.pid} tab={route.tab} focus={route.focus} />}
            {route.view === 'grants' && <GrantsView />}
            {route.view === 'daemon' && <DaemonView />}
          </main>
        </div>
      </div>
    </Ctx.Provider>
  );
}

function TopBar() {
  const { health, healthError } = useApp();
  return (
    <header className="flex h-12 shrink-0 items-center gap-4 border-b border-line bg-panel px-4">
      <div className="flex items-center gap-2 font-semibold">
        <span className="inline-flex size-6 items-center justify-center rounded-md bg-accent text-[13px] text-on-accent">◆</span>
        WrongMemory
      </div>
      <div className="ml-auto flex items-center gap-4 text-xs text-muted">
        {health ? (
          <>
            <span className="flex items-center gap-1.5 text-fg">
              <span className="size-2 rounded-full bg-ok shadow-[0_0_8px_var(--ok)]" /> Daemon running
            </span>
            <span>v{health.daemonVersion}</span>
            <span>up {duration(health.uptimeMs)}</span>
            <span className="hidden md:inline">{health.openStores} open stores</span>
            <span className="hidden md:inline">{health.connections} connections</span>
          </>
        ) : (
          <span className="flex items-center gap-1.5 text-bad">
            <span className="size-2 rounded-full bg-bad" /> {healthError ? 'Daemon unreachable' : 'Connecting…'}
          </span>
        )}
      </div>
    </header>
  );
}

function Sidebar() {
  const { projects, stats, route, go } = useApp();
  const [showEmpty, setShowEmpty] = useState(false);
  const sorted = useMemo(() => {
    const total = (id: string) => sum(statsOf(stats[id])?.byStatus);
    return [...projects].sort((a, b) => total(b.projectId) - total(a.projectId) || (a.displayName ?? '').localeCompare(b.displayName ?? ''));
  }, [projects, stats]);
  const visible = sorted.filter((p) => showEmpty || sum(statsOf(stats[p.projectId])?.byStatus) > 0);
  const hidden = sorted.length - visible.length;

  const item = (active: boolean) =>
    cx(
      'flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-[13px] transition-colors',
      active ? 'bg-accent-soft text-fg' : 'text-muted hover:bg-raised hover:text-fg',
    );

  return (
    <nav className="flex w-64 shrink-0 flex-col gap-4 overflow-y-auto border-r border-line bg-panel px-3 py-4">
      <button type="button" className={item(route.view === 'home')} onClick={() => go({ view: 'home' })}>
        <House className="size-4" /> All projects
        <span className="ml-auto text-xs text-faint">{projects.length}</span>
      </button>

      <div>
        <div className="mb-1 px-2.5 text-[11px] font-semibold uppercase tracking-wide text-faint">Projects</div>
        <ul className="flex flex-col gap-0.5">
          {visible.map((p) => {
            const st = statsOf(stats[p.projectId]);
            const active = route.view === 'project' && route.pid === p.projectId;
            return (
              <li key={p.projectId}>
                <button
                  type="button"
                  title={p.root}
                  className={item(active)}
                  onClick={() => go({ view: 'project', pid: p.projectId, tab: 'overview' })}
                >
                  <Database className={cx('size-4 shrink-0', p.admission === 'read-only' ? 'text-warn' : 'text-faint')} />
                  <span className="min-w-0 flex-1 truncate">{p.displayName || p.projectId}</span>
                  <span className="text-xs tabular-nums text-faint">{st?.byStatus?.active ?? 0}</span>
                </button>
              </li>
            );
          })}
        </ul>
        {hidden > 0 && (
          <button type="button" className="mt-1 px-2.5 text-xs text-faint hover:text-muted" onClick={() => setShowEmpty(true)}>
            + {hidden} empty project{hidden === 1 ? '' : 's'}
          </button>
        )}
        {showEmpty && hidden === 0 && sorted.some((p) => sum(statsOf(stats[p.projectId])?.byStatus) === 0) && (
          <button type="button" className="mt-1 px-2.5 text-xs text-faint hover:text-muted" onClick={() => setShowEmpty(false)}>
            Hide empty projects
          </button>
        )}
      </div>

      <div className="mt-auto">
        <div className="mb-1 px-2.5 text-[11px] font-semibold uppercase tracking-wide text-faint">Service</div>
        <button type="button" className={item(route.view === 'grants')} onClick={() => go({ view: 'grants' })}>
          <KeyRound className="size-4" /> Client grants
        </button>
        <button type="button" className={item(route.view === 'daemon')} onClick={() => go({ view: 'daemon' })}>
          <Server className="size-4" /> Daemon
        </button>
      </div>
    </nav>
  );
}
