/**
 * WM-001 / WM-002 / WM-004 (seed): build SAGE oracle fixtures with the real
 * WrongStack SAGE store from a clean oracle (prepare-clean-wrongstack.ts).
 *
 * Writes:
 *   tests/fixtures/oracle/<fixture>/sage.db        checkpointed (WAL-mode header, no -wal)
 *   tests/fixtures/oracle/<fixture>/manifest.json  SAGE record manifest written by the store
 *   tests/fixtures/oracle/<fixture>/record.json    every stored row (canonical persistence)
 *   tests/fixtures/oracle/<fixture>/queries.json   real API outputs for fixed query vectors
 *   contracts/profiles/sage-sqlite-v5-baseline/{schema.sql,manifest.json}
 *   contracts/fixtures/record-shape.json           vocabularies, limits, defaults, KATs
 *
 * Determinism: the store clock, `Date.now`, and `crypto.randomBytes` (ULID
 * randomness) are replaced with deterministic sources before SAGE is loaded,
 * so re-running produces byte-identical JSON and identical ids. Stores are
 * built under a fixed path (`C:/wrongmemory-oracle/<fixture>`) so nothing
 * user-specific is embedded.
 *
 * Usage: bun scripts/oracle/build-fixtures.ts --wrongstack <clean-oracle-dir> [--work-root <dir>]
 */
import crypto, { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { syncBuiltinESMExports } from 'node:module';
import { join, resolve } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/oracle/build-fixtures.ts --wrongstack <clean-oracle-dir> [--work-root <dir>]');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const workRoot = resolve(
  arg('--work-root') ?? (process.platform === 'win32' ? 'C:/wrongmemory-oracle' : '/tmp/wrongmemory-oracle'),
);
const baseline = JSON.parse(readFileSync(join(repo, 'contracts/baseline/wrongstack-baseline.json'), 'utf8')) as {
  commit: string;
};
const markerPath = join(resolve(wrongstack), '.wrongmemory-oracle.json');
if (!existsSync(markerPath)) {
  console.error(`${wrongstack} is not a clean oracle (missing .wrongmemory-oracle.json); run oracle:prepare`);
  process.exit(1);
}
const oracle = JSON.parse(readFileSync(markerPath, 'utf8')) as Record<string, unknown> & { commit: string };
if (oracle.commit !== baseline.commit) {
  console.error(`oracle commit ${oracle.commit} does not match baseline ${baseline.commit}`);
  process.exit(1);
}

// ─── Deterministic clock and randomness (before SAGE loads) ─────────────────
const EPOCH = Date.parse('2026-11-01T00:00:00.000Z');
let clock = EPOCH;
const tick = (seconds = 1) => {
  clock += seconds * 1000;
};
Date.now = () => clock;
let randomCounter = 0;
(crypto as unknown as { randomBytes: (n: number) => Buffer }).randomBytes = (len: number) => {
  const h = createHash('sha256').update(`wrongmemory-oracle:${randomCounter++}`).digest();
  const out = Buffer.alloc(len);
  for (let i = 0; i < len; i++) out[i] = h[i % h.length]!;
  return out;
};
syncBuiltinESMExports();

const sageSrc = join(resolve(wrongstack), 'packages/sage/src');
const { SqliteSageStore } = await import(join(sageSrc, 'sqlite-store.ts'));
const memoryModel = await import(join(sageSrc, 'memory-model.ts'));
const memoryText = await import(join(sageSrc, 'memory-text.ts'));
const searchHelpers = await import(join(sageSrc, 'sqlite-store-search-helpers.ts'));
const inputValidation = await import(join(sageSrc, 'memory-input-validation.ts'));
const schemaModule = await import(join(sageSrc, 'sqlite-store-schema.ts'));
const typesModule = await import(join(sageSrc, 'types.ts'));
const textIndexModule = await import(join(sageSrc, 'sqlite-store-text-index.ts'));
const paginationModule = await import(join(sageSrc, 'shared/pagination.ts'));

type Row = Record<string, unknown>;
type Store = InstanceType<typeof SqliteSageStore>;

function stable(value: unknown): string {
  return `${JSON.stringify(value, (_k, v) => (v === undefined ? undefined : v), 2)}\n`;
}

/**
 * ULID randomness (node:crypto randomBytes imported natively by the bundled
 * core dist) and SQLite randomblob() (HQ change ids / sync epoch) cannot be
 * made deterministic from outside. Those values are noncontractual, so the
 * committed JSON replaces each known id with `<id:label>`, any other ULID with
 * `<ulid:n>` (first-appearance order), and randomblob fields with
 * `<volatile>`. Real ids are written to `ids.json` beside the fixture.
 */
const VOLATILE_KEYS = new Set(['change_id', 'epoch']);
function normalizeVolatile(value: unknown, ids: Record<string, string>): string {
  const byId = new Map(Object.entries(ids).map(([label, id]) => [id.replace(/^candidate_/, ''), label]));
  const unknown = new Map<string, string>();
  const text = JSON.stringify(
    value,
    (k, v) => {
      if (v === undefined) return undefined;
      if (VOLATILE_KEYS.has(k) && typeof v === 'string') return '<volatile>';
      // Page cursors are base64url(JSON {u: updatedAt, i: id}); decode so the id inside normalizes.
      if (k === 'nextCursor' && typeof v === 'string') return `<cursor:${Buffer.from(v, 'base64url').toString('utf8')}>`;
      return v;
    },
    2,
  );
  return `${text.replace(/01[0-9A-HJKMNP-TV-Z]{24}/g, (u) => {
    const label = byId.get(u);
    if (label) return `<id:${label}>`;
    if (!unknown.has(u)) unknown.set(u, `<ulid:${unknown.size + 1}>`);
    return unknown.get(u)!;
  })}\n`;
}

function newStore(root: string): Store {
  return new SqliteSageStore({ projectRoot: root, now: () => new Date(clock) });
}

function dbPathFor(root: string) {
  return join(root, '.wrongstack/memories/sage.db');
}

/** Checkpoint so the single `sage.db` file is complete, then close. */
function checkpoint(dbFile: string) {
  const db = new DatabaseSync(dbFile);
  db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
  db.close();
}

function dumpRecords(dbFile: string) {
  const db = new DatabaseSync(dbFile, { readOnly: true });
  const all = (sql: string) => db.prepare(sql).all() as Row[];
  const tables = all("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'memories_fts%' ORDER BY name").map(
    (r) => String(r.name),
  );
  const out: Record<string, Row[]> = {};
  for (const t of tables) {
    const order = t === 'memories' ? 'rowid' : t === 'edges' ? 'from_node, to_node, relation' : t === 'audit_log' ? 'id' : '1';
    out[t] = all(`SELECT ${t === 'memories' ? 'rowid AS _rowid, ' : ''}* FROM ${t} ORDER BY ${order}`);
  }
  // FTS index content for parity checks. The external-content columns
  // (text/tags/audience) do not exist on `memories`, so reading them through
  // memories_fts fails; the index itself is dumped through fts5vocab.
  db.exec('CREATE VIRTUAL TABLE temp.fts_vocab USING fts5vocab(main, memories_fts, instance)');
  out.memories_fts_instances = all('SELECT term, doc AS _rowid, col, offset FROM temp.fts_vocab ORDER BY doc, col, offset, term');
  db.close();
  return out;
}

function schemaRows(dbFile: string) {
  const db = new DatabaseSync(dbFile, { readOnly: true });
  const rows = db
    .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")
    .all() as Array<{ type: string; name: string; tbl_name: string; sql: string | null }>;
  const tables = rows.filter((r) => r.type === 'table').map((r) => r.name);
  const columns: Record<string, unknown[]> = {};
  for (const t of tables) {
    columns[t] = (db.prepare(`PRAGMA table_xinfo(${JSON.stringify(t)})`).all() as Row[]).map((c) => ({
      name: c.name,
      type: c.type,
      notnull: c.notnull === 1,
      default: c.dflt_value ?? null,
      pk: c.pk,
      hidden: c.hidden,
    }));
  }
  const meta = db.prepare('SELECT key, value FROM schema_meta ORDER BY key').all() as Row[];
  const revisions = db.prepare('SELECT name, revision FROM ws_text_index_revisions ORDER BY name').all() as Row[];
  const pragmas = {
    journal_mode: (db.prepare('PRAGMA journal_mode').get() as Row).journal_mode,
    user_version: (db.prepare('PRAGMA user_version').get() as Row).user_version,
    application_id: (db.prepare('PRAGMA application_id').get() as Row).application_id,
    page_size: (db.prepare('PRAGMA page_size').get() as Row).page_size,
    auto_vacuum: (db.prepare('PRAGMA auto_vacuum').get() as Row).auto_vacuum,
    encoding: (db.prepare('PRAGMA encoding').get() as Row).encoding,
  };
  const sqliteVersion = (db.prepare('SELECT sqlite_version() AS v').get() as Row).v;
  const compileOptions = (db.prepare('PRAGMA compile_options').all() as Row[]).map((r) => r.compile_options);
  db.close();
  return { rows, columns, meta, revisions, pragmas, sqliteVersion, compileOptions };
}

const normalizeSql = (sql: string | null) => (sql ?? '').replace(/\s+/g, ' ').trim();

// ─── Fixture scripts ────────────────────────────────────────────────────────
interface FixtureContext {
  root: string;
  store: Store;
  ids: Record<string, string>;
}

type Query = { name: string; run: (ctx: FixtureContext) => Promise<unknown> };

const PROJECT_SEARCHES = [
  'bun',
  'Bun scripts',
  'server start',
  'START',
  'nonexistentterm',
  '',
  'AND OR NOT',
  'a',
  'session',
  'review',
];

function searchQueries(extra: string[] = []): Query[] {
  const qs: Query[] = [];
  for (const q of [...PROJECT_SEARCHES, ...extra]) {
    qs.push({ name: `searchSage(${JSON.stringify(q)})`, run: (c) => c.store.searchSage(q) });
  }
  qs.push(
    { name: 'searchSage("session", sessionId=sess-A)', run: (c) => c.store.searchSage('session', { sessionId: 'sess-A' }) },
    { name: 'searchSage("session", sessionId=sess-B)', run: (c) => c.store.searchSage('session', { sessionId: 'sess-B' }) },
    { name: 'searchSage("session", includeAllSessions)', run: (c) => c.store.searchSage('session', { includeAllSessions: true }) },
    {
      name: 'searchSage("", includeStatuses=all, includeAllSessions)',
      run: (c) =>
        c.store.searchSage('', {
          includeStatuses: [...memoryModel.SAGE_STATUSES],
          includeAllSessions: true,
          limit: 100,
        }),
    },
    { name: 'searchSage("reviewer", includeAudienceScoped=false)', run: (c) => c.store.searchSage('reviewer', { includeAudienceScoped: false }) },
    { name: 'searchSage("bun", limit=1)', run: (c) => c.store.searchSage('bun', { limit: 1 }) },
    { name: 'searchSage("convention scripts", requireAllTerms)', run: (c) => c.store.searchSage('convention scripts', { requireAllTerms: true }) },
    { name: 'searchSage("bun", scope=user)', run: (c) => c.store.searchSage('bun', { scope: 'user' }) },
  );
  return qs;
}

const COMMON_QUERIES: Query[] = [
  ...searchQueries(),
  { name: 'retrieveForPath([src/app.ts])', run: (c) => c.store.retrieveForPath(['src/app.ts']) },
  { name: 'retrieveForPath([src/app.ts], sessionId=sess-A)', run: (c) => c.store.retrieveForPath(['src/app.ts'], { path: 'src/app.ts', sessionId: 'sess-A' }) },
  { name: 'retrieveForPath([src/lib/util.ts])', run: (c) => c.store.retrieveForPath(['src/lib/util.ts']) },
  { name: 'retrieveForPath([docs/none.md])', run: (c) => c.store.retrieveForPath(['docs/none.md']) },
  { name: 'listCandidates(false)', run: (c) => c.store.listCandidates(false) },
  { name: 'listCandidates(true)', run: (c) => c.store.listCandidates(true) },
  { name: 'listSagePage()', run: (c) => c.store.listSagePage() },
  { name: 'listSagePage(statuses=[deleted])', run: (c) => c.store.listSagePage({ statuses: ['deleted'] }) },
  { name: 'listSagePage(limit=2)', run: (c) => c.store.listSagePage({ limit: 2 }) },
  { name: 'listSagePage(sessionId=sess-A)', run: (c) => c.store.listSagePage({ sessionId: 'sess-A' }) },
  { name: 'getSage(unknown)', run: (c) => c.store.getSage('01ZZZZZZZZZZZZZZZZZZZZZZZZ') },
  { name: 'getStats()', run: (c) => c.store.getStats() },
  { name: 'graphFor("src/app.ts")', run: (c) => c.store.graphFor('src/app.ts') },
];

interface FixtureSpec {
  name: string;
  build: (ctx: FixtureContext) => Promise<void>;
  /** Raw SQL applied after the store is closed (corruption cases). */
  afterClose?: (db: DatabaseSync) => void;
  extraQueries?: Query[];
}

async function remember(ctx: FixtureContext, label: string, input: Record<string, unknown>) {
  tick();
  const m = await ctx.store.rememberSage(input);
  ctx.ids[label] = m.id;
  return m;
}

const FIXTURES: FixtureSpec[] = [
  { name: 'empty', build: async () => {} },
  {
    name: 'basic',
    build: async (ctx) => {
      await remember(ctx, 'convention', {
        text: 'Use bun run for repository scripts; never call npm directly in this project.',
        kind: 'convention',
        tags: ['tooling', '#Bun'],
        anchors: [{ type: 'file', path: 'src/app.ts' }],
        validity: {
          statement: 'Holds while package.json declares bun scripts.',
          checks: [{ type: 'source_contains', path: 'package.json', text: '"scripts"' }],
        },
        sources: [{ type: 'file', path: 'package.json' }],
      });
      await remember(ctx, 'decision', {
        text: 'The HTTP server start sequence binds the port before loading plugins.',
        kind: 'decision',
        importance: 0.9,
        confidence: 0.85,
        tags: ['server'],
        anchors: [{ type: 'symbol', path: 'src/app.ts', symbol: 'startServer' }],
      });
      await remember(ctx, 'command', {
        text: 'Run the watch test command with bun run test --watch for fast feedback.',
        kind: 'command_note',
        tags: ['testing'],
        anchors: [{ type: 'command', command: 'bun  run   test --watch' }],
      });
      await remember(ctx, 'audience', {
        text: 'Reviewer guidance: check error handling paths in every review.',
        kind: 'role_operational',
        audience: { roles: ['reviewer'], taskTypes: ['review'] },
        anchors: [{ type: 'directory', path: 'src/lib' }],
      });
      await remember(ctx, 'sessionA', {
        text: 'Session note: the current session is investigating flaky retry timing.',
        kind: 'fact',
        scope: 'session',
        ownerSessionId: 'sess-A',
      });
      await remember(ctx, 'user', {
        text: 'The user prefers concise English explanations with bun commands.',
        kind: 'preference',
        scope: 'user',
        persistence: 'permanent',
      });
      await remember(ctx, 'successor', {
        text: 'Use bun run for all repository scripts, including CI workflows.',
        kind: 'convention',
        tags: ['tooling'],
        supersedes: [ctx.ids.convention],
        anchors: [{ type: 'file', path: 'src/app.ts' }],
      });
      await remember(ctx, 'toDelete', {
        text: 'Temporary workaround: pin the fixture clock to UTC midnight.',
        kind: 'warning',
        anchors: [{ type: 'file', path: 'src/lib/util.ts' }],
      });
      tick();
      await ctx.store.deleteSage(ctx.ids.toDelete, 'oracle fixture tombstone', { force: true });
      tick();
      await ctx.store.updateSage(ctx.ids.decision, {
        expectedRevision: 1,
        feedback: {
          verdict: 'useful',
          observedRevision: 1,
          evidence: 'Confirmed in src/app.ts startServer.',
          sessionId: 'sess-A',
        },
      });
      tick();
      await ctx.store.updateSage(ctx.ids.command, { expectedRevision: 1, importance: 0.65, tags: ['testing', 'watch'] });
      tick();
      const cand = await ctx.store.createCandidate({
        text: 'Proposal: document the plugin loading order in README.',
        kind: 'workflow',
        tags: ['docs'],
        sources: [{ type: 'session', sessionId: 'sess-A' }],
      });
      ctx.ids.candidate = cand.id;
      tick();
      const review = await ctx.store.createCandidate({
        text: 'Review: the watch test command note may be outdated.',
        kind: 'memory_review',
        targetMemoryId: ctx.ids.command,
        targetRevision: 2,
        reviewReason: 'possibly_outdated',
        suggestedAction: 'investigate',
      });
      ctx.ids.reviewCandidate = review.id;
      tick();
      await ctx.store.recordInjection([ctx.ids.decision], 'oracle', 'sess-A');
      await ctx.store.recordUse([ctx.ids.decision], 'oracle', 'sess-A');
      await ctx.store.drainMutations();
    },
    // The API refuses a session-scoped write without ownerSessionId; legacy
    // stores can still hold unowned session rows (visible to unscoped calls).
    afterClose: (db) => {
      const id = '01LEGACYUNOWNEDSESSION0000';
      const data = {
        id,
        revision: 1,
        text: 'Legacy unowned session memory about session cleanup behavior.',
        kind: 'fact',
        scope: 'session',
        status: 'active',
        tags: [],
        anchors: [],
        sources: [{ type: 'legacy_memory' }],
        importance: 0.5,
        confidence: 0.6,
        freshness: 1,
        createdAt: '2026-11-01T00:00:30.000Z',
        updatedAt: '2026-11-01T00:00:30.000Z',
      };
      db.prepare(
        `INSERT INTO memories (id, data, status, kind, scope, legacy_scope, importance, confidence, freshness, updated_at, created_at, audience, tags, owner_session_id, canonical_text)
         VALUES (?, ?, 'active', 'fact', 'session', NULL, 0.5, 0.6, 1, ?, ?, NULL, '', NULL, ?)`,
      ).run(id, JSON.stringify(data), data.updatedAt, data.createdAt, data.text.toLowerCase());
    },
    extraQueries: [
      { name: 'getSage(each)', run: async (c) => Promise.all(Object.entries(c.ids).map(async ([k, id]) => [k, await c.store.getSage(id)])) },
      { name: 'findRelatedSage([successor])', run: (c) => c.store.findRelatedSage([c.ids.successor]) },
      { name: 'retrieveForAudience(role=reviewer)', run: (c) => c.store.retrieveForAudience({ role: 'reviewer', taskType: 'review' }) },
      { name: 'findMemoriesForFile(src/app.ts)', run: (c) => c.store.findMemoriesForFile('src/app.ts') },
    ],
  },
  {
    name: 'unicode',
    build: async (ctx) => {
      await remember(ctx, 'turkish', { text: 'İstanbul ŞEHİR çalışma dizini: Iğdır ve ığdır farklı yazılır.', kind: 'fact', tags: ['Türkçe'] });
      await remember(ctx, 'nfkc', { text: 'The ﬁle ligature and Ｆｕｌｌｗｉｄｔｈ text fold under NFKC ①.', kind: 'fact' });
      await remember(ctx, 'astral', { text: 'Rocket 🚀 launch notes with math 𝒜𝒷𝒸 letters and 𝟙𝟚𝟛 digits.', kind: 'fact' });
      await remember(ctx, 'crlf', { text: 'line one\r\nline two\r\n\twith tab   and spaces', kind: 'fact' });
      await remember(ctx, 'bom', { text: '\uFEFFBOM leading marker text in memory body', kind: 'fact' });
      await remember(ctx, 'german', { text: 'Straße und STRASSE: German sharp s folding check.', kind: 'fact' });
      await ctx.store.drainMutations();
    },
    extraQueries: [
      ...['istanbul', 'İstanbul', 'şehir', 'ŞEHİR', 'çalışma', 'file', 'fullwidth', 'Ｆｕｌｌ', '🚀', 'abc', '𝒜𝒷𝒸', 'line two', 'straße', 'strasse', 'BOM', '①', '1'].map(
        (q): Query => ({ name: `searchSage(${JSON.stringify(q)})`, run: (c) => c.store.searchSage(q) }),
      ),
      { name: 'getSage(each)', run: async (c) => Promise.all(Object.entries(c.ids).map(async ([k, id]) => [k, await c.store.getSage(id)])) },
    ],
  },
  {
    name: 'corrupt-row',
    build: async (ctx) => {
      await remember(ctx, 'valid', { text: 'A valid memory beside a corrupt sibling row.', kind: 'fact', tags: ['sibling'] });
      await ctx.store.drainMutations();
    },
    afterClose: (db) => {
      const ins = db.prepare(
        `INSERT INTO memories (id, data, status, kind, scope, legacy_scope, importance, confidence, freshness, updated_at, created_at, audience, tags, owner_session_id, canonical_text)
         VALUES (?, ?, 'active', 'fact', 'project', NULL, 0.5, 0.5, 1, '2026-11-01T00:00:10.000Z', '2026-11-01T00:00:10.000Z', NULL, ?, NULL, ?)`,
      );
      ins.run('01CORRUPTJSON0000000000000', '{"id":"01CORRUPTJSON0000000000000","text":"corrupt sibling memory', 'sibling', 'corrupt sibling memory');
      ins.run(
        '01CORRUPTSHAPE000000000000',
        JSON.stringify({ id: '01CORRUPTSHAPE000000000000', text: 'shape-invalid sibling memory', scope: 'project', kind: 'fact', status: 'active' }),
        'sibling',
        'shape-invalid sibling memory',
      );
      db.prepare(`INSERT INTO candidates (id, data, status, created_at, updated_at, canonical_text) VALUES (?, ?, 'pending', ?, ?, ?)`).run(
        '01CORRUPTCAND0000000000000',
        '{"broken": ',
        '2026-11-01T00:00:10.000Z',
        '2026-11-01T00:00:10.000Z',
        '',
      );
    },
    extraQueries: [
      { name: 'searchSage("sibling")', run: (c) => c.store.searchSage('sibling') },
      { name: 'searchSage("memory")', run: (c) => c.store.searchSage('memory') },
      { name: 'getSage(01CORRUPTJSON)', run: (c) => c.store.getSage('01CORRUPTJSON0000000000000') },
      { name: 'getSage(01CORRUPTSHAPE)', run: (c) => c.store.getSage('01CORRUPTSHAPE000000000000') },
      { name: 'getSage(valid)', run: (c) => c.store.getSage(c.ids.valid) },
    ],
  },
];

async function settle(fn: () => Promise<unknown>): Promise<unknown> {
  try {
    return { ok: await fn() };
  } catch (e) {
    const err = e as Error;
    return { error: { name: err?.name ?? 'Error', message: err?.message ?? String(e) } };
  }
}

// Silence SAGE's corrupt-row console.warn during runs but record them.
const warnings: string[] = [];
const origWarn = console.warn;
console.warn = (...a: unknown[]) => {
  warnings.push(a.map(String).join(' '));
};

const fixturesOut = join(repo, 'tests/fixtures/oracle');
let emptySchema: ReturnType<typeof schemaRows> | undefined;

for (const spec of FIXTURES) {
  clock = EPOCH;
  randomCounter = 0;
  warnings.length = 0;
  const root = join(workRoot, spec.name);
  rmSync(root, { recursive: true, force: true });
  mkdirSync(root, { recursive: true });
  const ctx: FixtureContext = { root, store: newStore(root), ids: {} };
  tick();
  await ctx.store.initialize();
  await spec.build(ctx);
  ctx.store.close();
  const dbFile = dbPathFor(root);
  if (spec.afterClose) {
    const db = new DatabaseSync(dbFile);
    spec.afterClose(db);
    db.close();
  }
  checkpoint(dbFile);

  // Snapshot bytes before any query: opening a store runs the initializer.
  const outDir = join(fixturesOut, spec.name);
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  copyFileSync(dbFile, join(outDir, 'sage.db'));
  copyFileSync(join(root, '.wrongstack/memories/manifest.json'), join(outDir, 'manifest.json'));
  const dbSha = createHash('sha256').update(readFileSync(join(outDir, 'sage.db'))).digest('hex');
  writeFileSync(join(outDir, 'ids.json'), stable({ fixture: spec.name, note: 'Volatile: real ids/hash of this build of sage.db; regenerated with the db.', sageDbSha256: dbSha, ids: ctx.ids }));
  writeFileSync(join(outDir, 'record.json'), normalizeVolatile({ fixture: spec.name, idLabels: Object.keys(ctx.ids), tables: dumpRecords(dbFile) }, ctx.ids));
  if (spec.name === 'empty') emptySchema = schemaRows(dbFile);

  // Queries against a reopened store over the same database.
  clock = EPOCH + 3_600_000;
  const qctx: FixtureContext = { root, store: newStore(root), ids: ctx.ids };
  await qctx.store.initialize();
  const results: Array<{ name: string; result: unknown }> = [];
  for (const q of [...COMMON_QUERIES, ...(spec.extraQueries ?? [])]) {
    results.push({ name: q.name, result: await settle(() => q.run(qctx)) });
  }
  qctx.store.close();
  writeFileSync(
    join(outDir, 'queries.json'),
    normalizeVolatile({
      fixture: spec.name,
      queryClock: new Date(EPOCH + 3_600_000).toISOString(),
      warnings: [...warnings],
      results,
    }, ctx.ids),
  );
  origWarn(`fixture ${spec.name}: ${Object.keys(ctx.ids).length} ids, ${results.length} queries, db ${dbSha.slice(0, 12)}`);
}
console.warn = origWarn;

// ─── Storage profile ────────────────────────────────────────────────────────
if (!emptySchema) throw new Error('empty fixture missing');
const profileDir = join(repo, 'contracts/profiles/sage-sqlite-v5-baseline');
mkdirSync(profileDir, { recursive: true });
const normRows = emptySchema.rows.map((r) => ({ type: r.type, name: r.name, tbl_name: r.tbl_name, sql: normalizeSql(r.sql) }));
const fingerprint = createHash('sha256').update(JSON.stringify(normRows)).digest('hex');
const schemaSql = [
  '-- SAGE storage profile sage-sqlite-v5-baseline.',
  `-- Generated by scripts/oracle/build-fixtures.ts from a fresh store at WrongStack ${baseline.commit}.`,
  '-- Exact sqlite_master rows (ORDER BY type, name); do not hand-edit.',
  '',
  ...emptySchema.rows.map((r) => `-- ${r.type} ${r.name} ON ${r.tbl_name}\n${r.sql ? `${r.sql};` : '-- (no sql: auto-created)'}\n`),
].join('\n');
writeFileSync(join(profileDir, 'schema.sql'), schemaSql);
writeFileSync(
  join(profileDir, 'manifest.json'),
  stable({
    schema: 'wrongmemory.storage-profile/1',
    profileId: 'sage-sqlite-v5-baseline',
    source: { repository: 'https://github.com/WrongStack/WrongStack', commit: baseline.commit, oracle: { archiveSha256: oracle.archiveSha256, distSha256: oracle.distSha256, bunLockSha256: oracle.bunLockSha256 } },
    ddlFingerprint: { algorithm: 'sha256(JSON of [{type,name,tbl_name,sql whitespace-collapsed}] ORDER BY type,name)', value: fingerprint },
    sqliteSchemaVersion: schemaModule.SQLITE_SCHEMA_VERSION,
    recordSchemaVersion: typesModule.SAGE_SCHEMA_VERSION,
    textIndex: textIndexModule.sageTextIndex,
    schemaMeta: Object.fromEntries(emptySchema.meta.map((r) => [r.key, r.value])),
    textIndexRevisions: emptySchema.revisions,
    tables: emptySchema.rows.filter((r) => r.type === 'table').map((r) => r.name),
    indexes: emptySchema.rows.filter((r) => r.type === 'index').map((r) => ({ name: r.name, table: r.tbl_name, autoIndex: r.sql === null })),
    triggers: emptySchema.rows.filter((r) => r.type === 'trigger').map((r) => ({ name: r.name, table: r.tbl_name })),
    columns: emptySchema.columns,
    pragmasObservedOnFile: emptySchema.pragmas,
    connectionPragmasSetByStore: {
      journal_mode: 'WAL',
      synchronous: 'NORMAL',
      busy_timeout: 30000,
      temp_store: 'MEMORY',
      foreign_keys: 'ON',
      cache_size: 'SageCachePragmas() (negative KiB)',
      mmap_size: 'SageCachePragmas() bytes',
    },
    auditLog: { maxRows: schemaModule.AUDIT_LOG_MAX_ROWS, pruneInterval: schemaModule.AUDIT_LOG_PRUNE_INTERVAL },
    runtime: { bun: Bun.version, sqliteVersion: emptySchema.sqliteVersion, compileOptions: emptySchema.compileOptions },
  }),
);

// ─── Record shape, vocabularies, limits, KATs ───────────────────────────────
const KAT_TEXTS = [
  'Use bun',
  '  Use   BUN\tscripts\n',
  'İstanbul ŞEHİR',
  'Iğdır ığdır',
  'ﬁle Ｆｕｌｌｗｉｄｔｈ ①',
  'Straße STRASSE',
  'line one\r\nline two',
  '\uFEFFBOM lead',
  'Rocket 🚀 𝒜𝒷𝒸 𝟙𝟚𝟛',
  'ǅungla ΣΊΣΥΦΟΣ',
  'Use pnpm.',
  'C++ foo.bar edge-case snake_case',
  '\u00A0nbsp\u2003em\u3000ideographic',
  '',
];
const QUERY_KATS = ['bun scripts', 'AND OR NOT', 'a', 'foo-bar baz.qux', '"quoted" (paren) col:umn*', 'İstanbul çalışma', '🚀 rocket', 'x'.repeat(3) + ' 12 3', Array.from({ length: 70 }, (_, i) => `t${i}`).join(' ')];
const sageTextFold = (v: unknown) => (typeof v === 'string' ? v.normalize('NFKC').toLowerCase() : '');
mkdirSync(join(repo, 'contracts/fixtures'), { recursive: true });

writeFileSync(
  join(repo, 'contracts/fixtures/record-shape.json'),
  stable({
    schema: 'wrongmemory.record-shape/1',
    source: { repository: 'https://github.com/WrongStack/WrongStack', commit: baseline.commit },
    recordSchemaVersion: typesModule.SAGE_SCHEMA_VERSION,
    sageFields: [
      'id', 'revision', 'feedback', 'validity', 'scope', 'legacyScope', 'kind', 'status', 'contextPolicy', 'persistence',
      'text', 'summary', 'importance', 'confidence', 'freshness', 'tags', 'anchors', 'audience', 'sources', 'supersedes',
      'supersededBy', 'contradicts', 'createdAt', 'updatedAt', 'lastAccessedAt', 'lastVerifiedAt', 'staleReason',
      'lastUsedAt', 'expiresAt', 'injectionCount', 'useCount', 'ownerSessionId',
    ],
    decodeRequiredFields: { id: 'string', text: 'string', scope: 'string', kind: 'string', status: 'string', importance: 'number', confidence: 'number', freshness: 'number', createdAt: 'present (unparseable -> 1970-01-01T00:00:00.000Z)', updatedAt: 'present (unparseable -> 1970-01-01T00:00:00.000Z)' },
    decodeDroppedWhenUnparseable: ['expiresAt', 'lastAccessedAt', 'lastVerifiedAt'],
    kinds: memoryModel.SAGE_KINDS,
    scopes: ['project', 'user', 'session', 'file', 'symbol'],
    statuses: memoryModel.SAGE_STATUSES,
    persistence: { values: [...typesModule.VALID_PERSISTENCE], default: typesModule.DEFAULT_PERSISTENCE },
    contextPolicy: ['eligible', 'never'],
    staleReason: ['verification', 'manual'],
    anchorTypes: ['file', 'directory', 'symbol', 'package', 'command', 'test', 'git', 'agent'],
    sourceTypes: ['user', 'session', 'tool_result', 'project_instruction', 'file', 'test', 'command', 'legacy_memory'],
    graphRelations: ['about_file', 'about_directory', 'about_symbol', 'about_package', 'about_command', 'about_agent', 'derived_from', 'validated_by', 'invalidated_by', 'supersedes', 'contradicts', 'related_to', 'same_topic'],
    candidateStatuses: ['pending', 'accepted', 'rejected', 'merged'],
    candidateSuggestedActions: ['delete', 'archive', 'keep', 'investigate', 'update', 'consolidate'],
    feedbackVerdicts: ['useful', 'outdated', 'incorrect', 'irrelevant', 'uncertain'],
    limits: {
      maxTextChars: inputValidation.MAX_MEMORY_TEXT_CHARS,
      maxMetadataItems: inputValidation.MAX_MEMORY_METADATA_ITEMS,
      maxFeedbackHistory: 8,
      validityStatementChars: [1, 1000],
      validityMaxChecks: 4,
      validityCheckPathMaxChars: 500,
      validityCheckTextChars: [1, 400],
      ftsMaxPrefixTerms: 64,
      defaultListLimit: paginationModule.DEFAULT_LIST_LIMIT,
    },
    rememberDefaults: {
      id: 'ulid(): 10 Crockford base32 chars of Date.now() ms + 16 chars of randomBytes(16)[i] % 32',
      revision: 1,
      status: 'active',
      scope: "input.scope ?? legacyToSageScope(input.legacyScope ?? 'project-memory') -> 'project'",
      kind: "input.kind ?? legacyTypeToKind(input.type) -> 'fact'",
      importance: 'clamp01(input.importance ?? importanceFromPriority(priority)); priority critical 1, high 0.8, medium/undefined 0.6, low 0.3; defaulted value capped by quality.importanceCap',
      confidence: 'clamp01(input.confidence ?? 0.8); defaulted value capped by quality.confidenceCap',
      freshness: 'clamp01(input.freshness ?? 1)',
      sources: "normalizeSources(input.sources ?? [{ type: 'user' }])",
      tags: "dedupe(tag.replace(/^#/, '').trim().toLowerCase()).filter(Boolean)",
      text: "normalizeText: text.replace(/\\s+/g, ' ').trim(); empty -> error",
      canonical_text: 'normalizeTextKey(normalizedText)',
      qualityCaps: {
        short_text: 'text.length < 12 -> conf<=0.55 imp<=0.55',
        few_tokens: 'tokenize(text).length < 3 -> conf<=0.6 imp<=0.6',
        unanchored: 'no anchors -> conf<=0.75; and if scope not session/user -> imp<=0.7',
        root_cause_unanchored: 'bug_root_cause without anchors -> conf<=0.65 imp<=0.75',
        ephemeral_pattern: 'conf<=0.35 imp<=0.35',
        progress_log: 'imp<=0.55',
      },
    },
    kats: {
      normalizeText: KAT_TEXTS.map((t) => ({ input: t, output: memoryText.normalizeText(t) })),
      normalizeTextKey: KAT_TEXTS.map((t) => ({ input: t, output: memoryText.normalizeTextKey(t) })),
      canonicalMemoryText: KAT_TEXTS.map((t) => ({ input: t, output: memoryText.canonicalMemoryText(t) })),
      unicodeTextFold_nfkc_lower_v1: KAT_TEXTS.map((t) => ({ input: t, output: sageTextFold(t) })),
      tokenize: KAT_TEXTS.map((t) => ({ input: t, output: memoryText.tokenize(t) })),
      ftsPrefixTerms: QUERY_KATS.map((q) => ({ input: q, output: searchHelpers.ftsPrefixTerms(q) })),
      sqliteNormalizeCommand: ['bun  run   test --watch', '  ＢＵＮ  Test ', 'git\tstatus\n-s'].map((c) => ({
        input: c,
        output: schemaModule.sqliteNormalizeCommand(c),
        family: schemaModule.sqliteCommandFamily(c),
      })),
    },
  }),
);
console.error(`profile sage-sqlite-v5-baseline fingerprint ${fingerprint}`);
