/**
 * WM-003: inventory of every SAGE store owner, writer, and capability consumer.
 *
 * Production ownership takeover requires that EVERY WrongStack path able to
 * open or mutate `sage.db` honors external ownership (architecture §7). This
 * script enumerates candidates at the immutable baseline commit with
 * `git grep <commit>` across the whole monorepo (test files excluded):
 *
 *   1. Direct store construction (opening runs the schema initializer and
 *      migrations, so construction itself is a mutation).
 *   2. Project-server daemon ownership/spawn and project-port clients.
 *   3. MCP surfaces (standalone sage-mcp, attached server, connect planner).
 *   4. Mutating SAGE method call sites, by method name.
 *   5. MemoryPort/MemoryStore container consumers and capability accessors.
 *
 * Name-based classification is a heuristic REVIEW LIST: each site carries the
 * evidence (whether the file imports SAGE code) so a reviewer can confirm it.
 *
 * Usage: bun scripts/inventory/writers.ts --wrongstack <checkout> [--commit <sha>]
 */
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const checkout = arg('--wrongstack');
if (!checkout) {
  console.error('usage: bun scripts/inventory/writers.ts --wrongstack <checkout> [--commit <sha>]');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const baseline = JSON.parse(readFileSync(join(repo, 'contracts/baseline/wrongstack-baseline.json'), 'utf8')) as {
  commit: string;
};
const commit = arg('--commit') ?? baseline.commit;

type Hit = { path: string; line: number; text: string };
function gitGrep(pattern: string): Hit[] {
  const r = Bun.spawnSync(
    [
      'git', 'grep', '-n', '-I', '-E', pattern, commit, '--',
      '*.ts', '*.tsx', '*.mts', '*.cts', '*.js', '*.mjs',
      ':!**/tests/**', ':!**/test/**', ':!**/*.test.ts', ':!**/*.spec.ts', ':!**/__tests__/**', ':!**/dist/**',
    ],
    { cwd: checkout, stdout: 'pipe', stderr: 'pipe' },
  );
  if (r.exitCode !== 0 && r.exitCode !== 1) throw new Error(r.stderr.toString());
  return r.stdout
    .toString()
    .split('\n')
    .filter(Boolean)
    .map((l) => {
      const rest = l.slice(commit.length + 1);
      const m = /^(.*?):(\d+):(.*)$/.exec(rest)!;
      return { path: m[1]!, line: Number(m[2]), text: m[3]!.trim().slice(0, 180) };
    });
}

const fileCache = new Map<string, string>();
function fileText(path: string): string {
  if (!fileCache.has(path)) {
    const r = Bun.spawnSync(['git', 'show', `${commit}:${path}`], { cwd: checkout, stdout: 'pipe' });
    fileCache.set(path, r.stdout.toString());
  }
  return fileCache.get(path)!;
}
const inSage = (p: string) => p.startsWith('packages/sage/src/');
const importsSage = (p: string) =>
  inSage(p) || /@wrongstack\/sage(-mcp)?['"/]|from '\.{1,2}\/.*sage|SqliteSageStore|MemoryPort/.test(fileText(p));
const packageOf = (p: string) => /^(packages|apps)\/([^/]+)/.exec(p)?.slice(1, 3).join('/') ?? p.split('/')[0]!;

/** Category -> { pattern, access }. Access is the default classification. */
const CATEGORIES: Record<string, { pattern: string; access: 'owner' | 'write' | 'read' | 'client' | 'mcp'; note: string }> = {
  'store-construction': {
    pattern: String.raw`new (SqliteSageStore|SqliteMemoryPort)\(|createSqliteMemoryPort\(`,
    access: 'owner',
    note: 'Opens sage.db and runs initSchema + v2..v5 migrations + JSONL import: construction is a mutation.',
  },
  'raw-sage-db': {
    pattern: String.raw`['"\`]sage\.db['"\`]|sage\.db-wal|DatabaseSync\(`,
    access: 'owner',
    note: 'Direct database path / handle; review whether it bypasses the store.',
  },
  'project-server': {
    pattern: String.raw`project-server(\.js|-[a-z-]+)?['"]|SageProjectServerConnection|spawnDetachedServer|resolveProjectServerUrl|isSageProjectServerAvailable|startSageProjectServer|runProjectServer`,
    access: 'owner',
    note: 'Per-project SAGE daemon (the current single owner) and its launch/connection paths.',
  },
  'project-port-client': {
    pattern: String.raw`createProjectSageMemoryPort\(|new ProjectSageMemoryPort\(`,
    access: 'client',
    note: 'Host-side port over project IPC; mutations happen in the project server.',
  },
  'mcp-surface': {
    pattern: String.raw`createSageMcpServer|createSageMcpToolHost|createAttachedSageMcp|serveAttachedSageMcpStdio|@wrongstack/sage-mcp|wstack-sage-mcp|sageConnect|planSageConnect`,
    access: 'mcp',
    note: 'External MCP tool surfaces and client configuration planners.',
  },
  'container-binding': {
    pattern: String.raw`TOKENS\.(MemoryStore|MemoryPort)|getSageService\(|getSageRetrieval\(|getSageSurface\(|requireSageService\(`,
    access: 'read',
    note: 'MemoryPort/MemoryStore resolution and capability accessors (consumer may then write).',
  },
};

const MUTATING_METHODS = [
  'rememberSage', 'updateSage', 'deleteSage', 'hardDeleteSage', 'recoverSage', 'backfillRecoverable',
  'createCandidate', 'addCandidate', 'acceptCandidate', 'rejectCandidate', 'resolveCandidate',
  'recordInjection', 'recordUse', 'hygiene', 'verify', 'verifyForPaths', 'applyHqSync', 'importLegacy',
  'consolidateSession', 'addGraphEdge', 'remapAnchors', 'remapSagePaths', 'forget', 'consolidate', 'clear',
];
const READ_METHODS = [
  'searchSage', 'searchSageWithBreakdown', 'retrieveForPath', 'findRelatedSage', 'retrieveForAudience', 'listSagePage',
  'getSage', 'listCandidates', 'graphFor', 'traverseGraph', 'listHqSync', 'getHqSyncVersion', 'unifiedSearchService',
  'findMemoriesForFile', 'readAudit', 'doctor', 'getStats',
];

const GENERIC_METHODS = new Set(['clear', 'verify', 'consolidate', 'forget', 'hygiene', 'getStats', 'doctor']);

type Site = Hit & { category: string; access: string; evidence: 'imports-sage' | 'name-collision-suspected' };
const sites: Site[] = [];
for (const [category, def] of Object.entries(CATEGORIES)) {
  for (const h of gitGrep(def.pattern)) {
    sites.push({ ...h, category, access: def.access, evidence: importsSage(h.path) ? 'imports-sage' : 'name-collision-suspected' });
  }
}
const methodSites = (names: string[], access: string) => {
  const pattern = String.raw`\.(${names.join('|')})\(`;
  const re = new RegExp(pattern);
  for (const h of gitGrep(pattern)) {
    const m = re.exec(h.text)?.[1] ?? '?';
    // Generic names (Map.clear, promise verify, ...) only count when the
    // receiver looks like a memory store/port on the same line.
    const generic = GENERIC_METHODS.has(m) && !/(memory|sage|store|port)\w*\??\.\w+\(/i.test(h.text);
    const evidence = !generic && importsSage(h.path) ? 'imports-sage' : 'name-collision-suspected';
    sites.push({ ...h, category: `.${m}()`, access, evidence });
  }
};
methodSites(MUTATING_METHODS, 'write');
methodSites(READ_METHODS, 'read');

sites.sort((a, b) => a.path.localeCompare(b.path) || a.line - b.line || a.category.localeCompare(b.category));
const confirmed = sites.filter((s) => s.evidence === 'imports-sage');
const summary: Record<string, Record<string, number>> = {};
for (const s of confirmed) {
  if (inSage(s.path)) continue;
  const pkg = packageOf(s.path);
  summary[pkg] ??= {};
  const key = `${s.access}:${s.category}`;
  summary[pkg][key] = (summary[pkg][key] ?? 0) + 1;
}
const writeCapablePackages = Object.entries(summary)
  .filter(([, cats]) => Object.keys(cats).some((k) => /^(owner|write|client|mcp):/.test(k)))
  .map(([pkg]) => pkg)
  .sort();

const out = {
  schema: 'wrongmemory.writer-inventory/1',
  wrongstackCommit: commit,
  method:
    'git grep at the baseline commit (tests/dist excluded). NAME-BASED REVIEW LIST: every site must be confirmed by a reviewer before WM-409/WM-600 ownership takeover.',
  categories: Object.fromEntries(Object.entries(CATEGORIES).map(([k, v]) => [k, { access: v.access, note: v.note, pattern: v.pattern }])),
  mutatingMethods: MUTATING_METHODS,
  readMethods: READ_METHODS,
  writeCapablePackagesOutsideSage: writeCapablePackages,
  summaryByPackage: Object.fromEntries(Object.entries(summary).sort(([a], [b]) => a.localeCompare(b))),
  counts: {
    sites: sites.length,
    insideSagePackage: sites.filter((s) => inSage(s.path)).length,
    suspectedNameCollisions: sites.length - confirmed.length,
  },
  sites,
};
mkdirSync(join(repo, 'contracts/inventory'), { recursive: true });
writeFileSync(join(repo, 'contracts/inventory/writers.json'), `${JSON.stringify(out, null, 2)}\n`);
console.error(
  `writers: ${sites.length} sites (${out.counts.insideSagePackage} inside packages/sage, ${out.counts.suspectedNameCollisions} suspected collisions); ` +
    `write-capable packages outside sage: ${writeCapablePackages.join(', ')}`,
);
