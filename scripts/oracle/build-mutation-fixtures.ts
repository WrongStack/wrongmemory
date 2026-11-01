/**
 * WM-004 (remainder) / WM-005: record SAGE mutation, lifecycle, review,
 * hygiene and graph behavior from the real SqliteSageStore of a clean oracle
 * (prepare-clean-wrongstack.ts) as deterministic scenario transcripts.
 *
 * Writes tests/fixtures/oracle-mutations/<scenario>.json and index.json.
 *
 * Each step records the call (method + input), its result or thrown error
 * (name + message), and a canonical dump of the store after the step:
 * memories (rowid order), edges, candidates, audit_log event names (new since
 * the previous step), hq_memory_sync (id, revision, data IS NULL) and
 * schema_meta.
 *
 * Determinism: Date.now and the store clock are a fixed, manually advanced
 * clock. ULID randomness and SQLite randomblob() cannot be fixed from outside
 * (the bundled core dist imports node:crypto natively), so every ULID is
 * replaced by `<id:label>` (labels assigned by the scenario) or `<ulid:n>`
 * (first appearance in the transcript); HQ change ids are dropped. Revisions,
 * statuses, timestamps, NULL-vs-absent and ordering are preserved. Arrays
 * whose SQL order depends on ids (edges, hq rows) are sorted after
 * normalization.
 *
 * Usage: bun scripts/oracle/build-mutation-fixtures.ts --wrongstack <clean-oracle-dir> [--work-root <dir>]
 */
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { DatabaseSync } from 'node:sqlite';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/oracle/build-mutation-fixtures.ts --wrongstack <clean-oracle-dir> [--work-root <dir>]');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const workRoot = resolve(
  arg('--work-root') ??
    (process.platform === 'win32' ? 'C:/wrongmemory-oracle-mutations' : '/tmp/wrongmemory-oracle-mutations'),
);
const outDir = join(repo, 'tests/fixtures/oracle-mutations');
const baseline = JSON.parse(readFileSync(join(repo, 'contracts/baseline/wrongstack-baseline.json'), 'utf8')) as {
  commit: string;
};
const markerPath = join(resolve(wrongstack), '.wrongmemory-oracle.json');
if (!existsSync(markerPath)) {
  console.error(`${wrongstack} is not a clean oracle; run oracle:prepare`);
  process.exit(1);
}
const oracle = JSON.parse(readFileSync(markerPath, 'utf8')) as { commit: string };
if (oracle.commit !== baseline.commit) {
  console.error(`oracle commit ${oracle.commit} does not match baseline ${baseline.commit}`);
  process.exit(1);
}

// ─── Deterministic clock (before SAGE loads) ────────────────────────────────
const EPOCH = Date.parse('2026-11-01T00:00:00.000Z');
let clock = EPOCH;
const tick = (seconds = 1) => {
  clock += seconds * 1000;
};
Date.now = () => clock;

const sageSrc = join(resolve(wrongstack), 'packages/sage/src');
const { SqliteSageStore } = await import(join(sageSrc, 'sqlite-store.ts'));
const inputValidation = await import(join(sageSrc, 'memory-input-validation.ts'));
const progressLog = await import(join(sageSrc, 'shared/progress-log.ts'));
const storeHelpers = await import(join(sageSrc, 'store-helpers.ts'));
const similarity = await import(join(sageSrc, 'memory-similarity.ts'));
const rerank = await import(join(sageSrc, 'retrieval/hybrid-rerank.ts'));

type Row = Record<string, unknown>;
type Store = InstanceType<typeof SqliteSageStore>;
const origWarn = console.warn;
const warnings: string[] = [];
console.warn = (...a: unknown[]) => {
  warnings.push(a.map(String).join(' '));
};

// ─── Scenario harness ───────────────────────────────────────────────────────
interface Step {
  step: string;
  call?: { method: string; input: unknown };
  result?: unknown;
  error?: { name: string; message: string };
  state?: unknown;
}

class Scenario {
  readonly root: string;
  readonly store: Store;
  readonly steps: Step[] = [];
  readonly labels = new Map<string, string>();
  private auditSeen = 0;

  constructor(readonly name: string, readonly description: string) {
    this.root = join(workRoot, name);
    rmSync(this.root, { recursive: true, force: true });
    mkdirSync(this.root, { recursive: true });
    this.store = new SqliteSageStore({ projectRoot: this.root, now: () => new Date(clock) });
  }

  label(id: string | undefined, name: string): string | undefined {
    if (id) this.labels.set(id.replace(/^(candidate_|mem_)/, ''), name);
    return id;
  }

  file(rel: string, content: string) {
    const p = join(this.root, rel);
    mkdirSync(dirname(p), { recursive: true });
    writeFileSync(p, content);
  }

  /** Run a store call, record result or error and the post-state. */
  async run<T>(step: string, method: string, input: unknown, fn: () => Promise<T> | T, opts: { state?: boolean } = {}): Promise<T | undefined> {
    tick();
    const entry: Step = { step, call: { method, input } };
    let value: T | undefined;
    try {
      value = await fn();
      entry.result = value === undefined ? '<undefined>' : value;
    } catch (error) {
      const e = error as Error;
      entry.error = { name: e?.name ?? 'Error', message: e?.message ?? String(error) };
    }
    if (opts.state !== false) entry.state = this.dump();
    this.steps.push(entry);
    return value;
  }

  /** Record a pure observation without a store mutation. */
  note(step: string, result: unknown) {
    this.steps.push({ step, result });
  }

  dbFile() {
    return join(this.root, '.wrongstack/memories/sage.db');
  }

  dump() {
    const db = new DatabaseSync(this.dbFile(), { readOnly: true });
    const all = (sql: string) => db.prepare(sql).all() as Row[];
    const memories = all(
      `SELECT id, status, kind, scope, legacy_scope, importance, confidence, freshness,
              owner_session_id, canonical_text, audience, tags, updated_at, created_at,
              unicode_text IS NULL AS unicode_text_pending, data
       FROM memories ORDER BY rowid`,
    ).map((r) => {
      let data: unknown = r.data;
      try {
        data = JSON.parse(String(r.data));
      } catch {
        /* keep raw corrupt text */
      }
      return { ...r, data };
    });
    const edges = all('SELECT from_node, to_node, relation, weight, created_at FROM edges');
    const candidates = all('SELECT id, status, created_at, updated_at, canonical_text, data FROM candidates ORDER BY rowid').map(
      (r) => ({ ...r, data: JSON.parse(String(r.data)) }),
    );
    const audit = all('SELECT id, event, data FROM audit_log ORDER BY id');
    const newAudit = audit.filter((r) => Number(r.id) > this.auditSeen);
    if (audit.length) this.auditSeen = Number(audit[audit.length - 1]!.id);
    const hq = all('SELECT id, revision, data IS NULL AS tombstone FROM hq_memory_sync');
    const hqClock = all('SELECT revision FROM hq_memory_sync_clock');
    const meta = all('SELECT key, value FROM schema_meta ORDER BY key');
    const textIndex = all('SELECT name, revision FROM ws_text_index_revisions ORDER BY name');
    db.close();
    return {
      memories,
      edges,
      candidates,
      auditEvents: newAudit.map((r) => {
        let data: unknown = r.data;
        try {
          data = JSON.parse(String(r.data));
        } catch {
          /* raw */
        }
        return { event: r.event, data };
      }),
      hq,
      hqClockRevision: hqClock[0]?.revision ?? null,
      schemaMeta: Object.fromEntries(meta.map((r) => [r.key, r.value])),
      textIndexRevisions: Object.fromEntries(textIndex.map((r) => [r.name, r.revision])),
    };
  }

  serialize(): string {
    const unknown = new Map<string, string>();
    const text = JSON.stringify(
      {
        scenario: this.name,
        description: this.description,
        baseline: baseline.commit,
        clockEpoch: new Date(EPOCH).toISOString(),
        steps: this.steps,
      },
      (k, v) => {
        if (v === undefined) return undefined;
        if (k === 'change_id' || k === 'epoch') return '<volatile>';
        // Fake credential samples exist only to exercise the secret guard; never
        // commit them verbatim (they trip secret scanners). Keep a stable tag.
        if (typeof v === 'string' && storeHelpers.looksLikeSecret(v)) {
          return `<fake-secret:${createHash('sha256').update(v).digest('hex').slice(0, 8)}>`;
        }
        if ((k === 'nextCursor' || k === 'cursor') && typeof v === 'string' && v !== 'not-a-cursor') {
          return `<cursor:${Buffer.from(v, 'base64url').toString('utf8')}>`;
        }
        return v;
      },
    );
    const normalized = text.replace(/01[0-9A-HJKMNP-TV-Z]{24}/g, (u) => {
      const label = this.labels.get(u);
      if (label) return `<id:${label}>`;
      if (!unknown.has(u)) unknown.set(u, `<ulid:${unknown.size + 1}>`);
      return unknown.get(u)!;
    });
    // Sort id-ordered arrays after normalization so random ULID suffixes do
    // not leak into ordering.
    const parsed = JSON.parse(normalized) as { steps: Step[] };
    for (const s of parsed.steps) {
      const st = s.state as { edges?: unknown[]; hq?: unknown[] } | undefined;
      if (st?.edges) st.edges.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
      if (st?.hq) st.hq.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
    }
    return `${JSON.stringify(parsed, null, 2)}\n`;
  }

  close() {
    try {
      this.store.close();
    } catch {
      /* ignore */
    }
  }
}

const scenarios: Scenario[] = [];
const idOf = (v: unknown) => (v as { id?: string } | undefined)?.id;

// ─── remember ───────────────────────────────────────────────────────────────
{
  const s = new Scenario('remember', 'rememberSage defaults, quality caps, merges, relationships, validation and rejections');
  scenarios.push(s);
  s.file('src/app.ts', 'export function startServer() {}\n');
  const r = (step: string, input: Row) => s.run(step, 'rememberSage', input, () => s.store.rememberSage(input));

  s.label(idOf(await r('defaults-unanchored', { text: 'The deploy pipeline publishes container images to the internal registry.' })), 'defaults');
  s.label(idOf(await r('anchored-tagged', {
    text: 'Use bun run for repository scripts; never call npm directly.',
    kind: 'convention',
    tags: ['#Tooling', ' bun ', 'tooling', ''],
    anchors: [{ type: 'file', path: './src/app.ts' }, { type: 'file', path: 'src\\app.ts' }],
  })), 'convention');
  s.label(idOf(await r('short-text-cap', { text: 'Use pnpm', anchors: [{ type: 'file', path: 'src/app.ts' }] })), 'short');
  s.label(idOf(await r('few-tokens-cap', { text: 'Prefer tabs in Go', anchors: [{ type: 'file', path: 'src/app.ts' }] })), 'fewTokens');
  s.label(idOf(await r('user-scope-unanchored', { text: 'The user prefers concise answers without bullet lists.', scope: 'user', kind: 'preference' })), 'userPref');
  s.label(idOf(await r('root-cause-unanchored', { text: 'Flaky retry tests were caused by a shared global clock mock leaking across suites.', kind: 'bug_root_cause' })), 'rootCause');
  s.label(idOf(await r('explicit-scores', { text: 'Release tags must be signed with the release key before publishing.', importance: 0.95, confidence: 0.99, freshness: 0.4, priority: 'low' })), 'explicit');
  s.label(idOf(await r('priority-high', { text: 'Database migrations run inside a single transaction per release.', priority: 'high' })), 'priorityHigh');
  s.label(idOf(await r('progress-log-cap', {
    text: 'Round r3 of 5 LANDED with suites 412/412 green at HEAD 1a2b3c4d; remaining rounds for future work.',
    scope: 'session',
    ownerSessionId: 'sess-A',
  })), 'progressLog');
  s.label(idOf(await r('ephemeral-session-allowed', { text: 'Still working on the parser refactor for the tokenizer module', scope: 'session', ownerSessionId: 'sess-A' })), 'ephemeralSession');
  await r('ephemeral-project-rejected', { text: 'Still working on the parser refactor for the tokenizer module' });
  await r('wip-rejected', { text: 'WIP refactor of the loader' });
  await r('todo-list-allowed-prefix', { text: 'todo lists are stored in the kanban board table per project.' });
  await r('too-short-rejected', { text: ' ab ' });
  await r('secret-rejected', { text: 'The api_key = sk-ant-abcdefghijklmnopqrstuvwxyz0123 is used in CI.' });
  await r('secret-in-tag-rejected', { text: 'Tokens live in the vault service.', tags: [['gh', 'p_', 'abcdefghijklmnopqrstuvwxyz0123456789'].join('')] });
  await r('session-without-owner-rejected', { text: 'Session-scoped note without an owner session id', scope: 'session' });
  await r('structural-kind-without-anchor-rejected', { text: 'This file holds the HTTP routes table.', kind: 'file_note' });
  await r('bad-kind-rejected', { text: 'Some durable fact about the system.', kind: 'galaxy' });
  await r('bad-persistence-rejected', { text: 'Some durable fact about the system.', persistence: 'forever' });
  await r('bad-anchor-rejected', { text: 'Some durable fact about the system.', anchors: [{ type: 'symbol', path: 'src/app.ts' }] });
  await r('validity-normalized', {
    text: 'Feature flags are read from config/flags.json at startup.',
    validity: { statement: '  Only while flags.json exists  ', checks: [{ type: 'source_contains', path: 'config\\flags.json', text: 'flags' }] },
  }).then((m) => s.label(idOf(m), 'validity'));
  await r('validity-absolute-path-rejected', { text: 'Feature flags are read from the config directory.', validity: { statement: 'x', checks: [{ type: 'source_contains', path: 'C:/x.json', text: 'a' }] } });
  await r('validity-unknown-field-rejected', { text: 'Feature flags are read from the config directory.', validity: { statement: 'x', extra: 1 } });
  // Exact duplicate merge (case/whitespace variant), with tag/anchor union and max scores.
  await r('exact-duplicate-merge', {
    text: '  use BUN run for repository scripts;   never call npm directly. ',
    kind: 'convention',
    tags: ['ci'],
    anchors: [{ type: 'command', command: 'bun  run   test' }],
    importance: 0.9,
  });
  // Same text but conditional validity: separate memory.
  await r('exact-text-different-validity-separate', {
    text: 'Use bun run for repository scripts; never call npm directly.',
    kind: 'convention',
    validity: { statement: 'Only in the monorepo root' },
  }).then((m) => s.label(idOf(m), 'conventionConditional'));
  // Near duplicate (paraphrase with more tokens) merges and prefers richer text.
  await r('near-duplicate-merge', {
    text: 'The deploy pipeline publishes container images to the internal registry after tests pass.',
  });
  // Polarity pair is not merged.
  await r('negated-not-merged', { text: 'The deploy pipeline does not publish container images to the internal registry.' }).then((m) =>
    s.label(idOf(m), 'negated'),
  );
  // Session duplicates across two sessions stay separate; same session merges.
  await r('session-A', { text: 'Session note: investigating flaky retry timing in the scheduler.', scope: 'session', ownerSessionId: 'sess-A' }).then((m) =>
    s.label(idOf(m), 'sessA'),
  );
  await r('session-B-same-text', { text: 'Session note: investigating flaky retry timing in the scheduler.', scope: 'session', ownerSessionId: 'sess-B' }).then((m) =>
    s.label(idOf(m), 'sessB'),
  );
  await r('session-A-repeat-merges', { text: 'Session note: investigating flaky retry timing in the scheduler.', scope: 'session', ownerSessionId: 'sess-A' });
  // Relationships.
  const conventionId = [...s.labels.entries()].find(([, l]) => l === 'convention')?.[0];
  await r('supersedes', {
    text: 'Use bun run for all repository scripts, including CI workflows and release jobs.',
    kind: 'convention',
    anchors: [{ type: 'file', path: 'src/app.ts' }],
    supersedes: [conventionId],
  }).then((m) => s.label(idOf(m), 'successor'));
  await r('contradicts', {
    text: 'Repository scripts are executed with npm run in the release pipeline only.',
    contradicts: [conventionId],
  }).then((m) => s.label(idOf(m), 'contradictor'));
  await r('symbol-agent-command-anchors', {
    text: 'startServer binds the port before plugins load; reviewers should check it.',
    kind: 'symbol_note',
    anchors: [
      { type: 'symbol', path: 'src/app.ts', symbol: ' startServer ' },
      { type: 'agent', role: ' Reviewer ' },
      { type: 'command', command: '  bun   test  --watch ' },
      { type: 'directory', path: 'src/lib/' },
    ],
    audience: { roles: [' Reviewer ', 'reviewer'], modes: [] },
  }).then((m) => s.label(idOf(m), 'symbol'));
  // Quality pattern outcomes (pure functions, recorded for porting).
  const texts = [
    'WIP refactor of the loader',
    'todo list sync is handled by the board service',
    'Still working on the parser',
    'need to fix the flaky test',
    'debugging the issue with retries',
    'fixed the bug.',
    'We decided to use bun for scripts',
    'Round r3 of 5 LANDED with suites 412/412 green at HEAD 1a2b3c4d',
    'rounds 4/9 complete; NO-PROVEN-BUG',
    'HEAD 0123abcd is the release base',
  ];
  s.note(
    'quality-pattern-outcomes',
    texts.map((text) => ({
      text,
      ephemeral: inputValidation.isEphemeralMemoryText(text),
      progressSignals: progressLog.progressLogSignals(text),
      progressLog: progressLog.isProgressLogText(text),
      quality: inputValidation.assessRememberQuality({ text, anchors: [], tags: [], scope: 'project' }),
      secret: storeHelpers.looksLikeSecret(text),
    })),
  );
  const secretSamples = [
    'password=hunter2hunter2hunter2',
    'token: abcdefghijklmnop1234',
    ['AK', 'IA', 'ABCDEFGHIJKLMNOP'].join(''),
    ['AI', 'za', 'SyA1234567890abcdefghijklmnopqrstuv'].join(''),
    ['hf', '_', 'abcdefghijklmnopqrstuvwxyz'].join(''),
    ['sk', '_li', 've_', 'abcdefghijklmnopqrstuvwx'].join(''),
    ['np', 'm_', 'abcdefghijklmnopqrstuvwxyz0123456789'].join(''),
    '-----BEGIN RSA PRIVATE KEY-----',
    ['eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9', 'eyJzdWIiOiIxMjM0NTY3ODkwIn0', 'dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U'].join('.'),
    'the token field is documented in README',
    'secretary of state',
  ];
  s.note('secret-pattern-outcomes', secretSamples.map((text) => ({ text, secret: storeHelpers.looksLikeSecret(text) })));
  const pairs: Array<[string, string]> = [
    ['The cache is invalidated on every deploy of the api service', 'The cache is not invalidated on every deploy of the api service'],
    ['The cache is invalidated on every deploy of the api service', 'The cache is invalidated on each deploy of the api service quickly'],
    ['Builds no longer use webpack for the frontend bundle step', 'Builds use webpack for the frontend bundle step'],
  ];
  s.note(
    'similarity-outcomes',
    pairs.map(([a, b]) => ({
      a,
      b,
      nearDuplicate: similarity.isNearDuplicateMemory({ text: a, kind: 'fact', anchors: [] }, { text: b, kind: 'fact', anchors: [] }),
      contradictory: similarity.isPossiblyContradictory({ text: a }, { text: b }),
    })),
  );
}

// ─── update ─────────────────────────────────────────────────────────────────
{
  const s = new Scenario('update', 'updateSage: field changes, revision guard, feedback, validity removal, immutability, status/staleReason');
  scenarios.push(s);
  s.file('src/app.ts', 'export const x = 1;\n');
  const base = await s.run('seed', 'rememberSage', { text: 'The HTTP server listens on port 8080 in development.' }, () =>
    s.store.rememberSage({ text: 'The HTTP server listens on port 8080 in development.', validity: { statement: 'Only in dev', checks: [{ type: 'source_contains', path: 'src/app.ts', text: 'x' }] } }),
  );
  const id = s.label(idOf(base), 'm')!;
  const other = s.label(idOf(await s.run('seed-successor', 'rememberSage', { text: 'The HTTP server listens on port 9090 in all environments now.' }, () =>
    s.store.rememberSage({ text: 'The HTTP server listens on port 9090 in all environments now.' }),
  )), 'successor')!;
  const u = (step: string, input: Row, target = id) => s.run(step, 'updateSage', { id: target, ...input }, () => s.store.updateSage(target, input));
  // Revision observed right now (feedback and expectedRevision must name it).
  const rev = async () => ((await s.store.getSage(id)) as { revision: number }).revision;
  await u('text-tags-kind', { text: '  The HTTP server listens on port 8081 in development. ', tags: ['#Net', 'net'], kind: 'decision' });
  await u('anchors', { anchors: [{ type: 'file', path: './src/app.ts' }] });
  await u('importance-confidence-freshness-clamp', { importance: 1.7, confidence: -2, freshness: 0.5 });
  await u('expected-revision-match', { expectedRevision: await rev(), persistence: 'permanent' });
  await u('expected-revision-mismatch', { expectedRevision: 1, text: 'Stale write attempt with an old revision number.' });
  await u('audience-set', { audience: { roles: ['Reviewer'], taskTypes: ['review'] } });
  await u('audience-clear', { audience: {} });
  await u('validity-null-removes', { validity: null });
  await u('sources-replace', { sources: [{ type: 'file', path: 'src\\app.ts' }, { type: 'file', path: 'src/app.ts' }] });
  await u('feedback-incorrect', { feedback: { verdict: 'incorrect', observedRevision: await rev(), evidence: ' Port is 9090 per config. ', sessionId: 's1' } });
  await u('feedback-duplicate-noop', { feedback: { verdict: 'incorrect', observedRevision: await rev(), evidence: 'Port is 9090 per config.', sessionId: 's1' } });
  await u('feedback-useful-same-revision', { feedback: { verdict: 'useful', observedRevision: await rev(), evidence: 'Helped locate the server config.' } });
  await u('feedback-wrong-revision', { feedback: { verdict: 'useful', observedRevision: 3, evidence: 'old' } });
  await u('feedback-bundled-rejected', { feedback: { verdict: 'useful', observedRevision: await rev(), evidence: 'x' }, text: 'Bundled content change is not allowed here.' });
  for (let i = 0; i < 8; i++) {
    await u(`feedback-cap-${i}`, { feedback: { verdict: 'uncertain', observedRevision: await rev(), evidence: `observation ${i}` } }, id);
  }
  await u('scope-ignored-immutable', { scope: 'session', ownerSessionId: 'sess-X', importance: 0.5 } as Row);
  await u('status-stale-manual', { status: 'stale' });
  await u('status-active-clears-staleReason', { status: 'active' });
  await u('superseded-by-without-status-rejected', { supersededBy: other });
  await u('superseded-by-self-rejected', { status: 'superseded', supersededBy: id });
  await u('superseded-by', { status: 'superseded', supersededBy: other });
  await u('leave-superseded-drops-pointer', { status: 'archived' });
  await u('delete-without-force-permanent-rejected', { status: 'deleted' });
  await u('delete-authorized-permanent-rejected', { status: 'deleted', deleteAuthorized: true });
  await u('ephemeral-text-rejected', { text: 'Still working on the port config' });
  await u('structural-kind-anchorless-rejected', { kind: 'file_note', anchors: [] });
  await u('invalid-status', { status: 'frozen' } as Row);
  await u('not-found', {}, '01NOTFOUNDNOTFOUNDNOTFOUND');
  await u('delete-force', { status: 'deleted', force: true });
}

// ─── delete / recover / backfill ────────────────────────────────────────────
{
  const s = new Scenario('delete-recover', 'deleteSage force gate, tombstone, reference cleanup, recoverSage, backfillRecoverable');
  scenarios.push(s);
  s.file('src/a.ts', 'a\n');
  const rem = async (label: string, input: Row) =>
    s.label(idOf(await s.run(`remember-${label}`, 'rememberSage', input, () => s.store.rememberSage(input))), label)!;
  const a = await rem('a', { text: 'Service A owns the billing tables and their migrations.', anchors: [{ type: 'file', path: 'src/a.ts' }] });
  const b = await rem('b', { text: 'Service B reads billing data only through the reporting API.', supersedes: [a] });
  const c = await rem('c', { text: 'Billing exports run nightly at 02:00 UTC from service A.', contradicts: [a] });
  const p = await rem('permanent', { text: 'Never store raw card numbers anywhere in the system.', persistence: 'permanent' });
  const d = (step: string, id: string, reason?: string, options?: Row) =>
    s.run(step, 'deleteSage', { id, reason, options }, () => s.store.deleteSage(id, reason, options));
  await d('delete-without-force', a);
  await d('delete-permanent-without-force', p);
  await d('delete-force', a, 'obsolete', { force: true });
  await d('delete-again-noop', a, 'again', { force: true });
  await d('delete-never-inject', c, 'noise', { force: true, neverInject: true });
  await s.run('recover-a', 'recoverSage', { id: a }, () => s.store.recoverSage(a));
  await s.run('recover-active-noop', 'recoverSage', { id: a }, () => s.store.recoverSage(a));
  await d('delete-a-again', a, 'again', { force: true });
  await s.run('backfill-dry-run', 'backfillRecoverable', {}, () => s.store.backfillRecoverable());
  const report = (await s.run('backfill-apply', 'backfillRecoverable', { dryRun: false }, () =>
    s.store.backfillRecoverable({ dryRun: false }),
  )) as { recoverableRecords?: Array<{ originalId: string; newActiveId?: string }> } | undefined;
  for (const rec of report?.recoverableRecords ?? []) {
    const lbl = s.labels.get(rec.originalId);
    if (rec.newActiveId && lbl) s.label(rec.newActiveId, `${lbl}Backfilled`);
  }
  await s.run('recover-after-backfill-returns-successor', 'recoverSage', { id: a }, () => s.store.recoverSage(a));
  await s.run('backfill-again-already-recovered', 'backfillRecoverable', { dryRun: false }, () => s.store.backfillRecoverable({ dryRun: false }));
  await s.run('get-tombstone', 'getSage', { id: c }, () => s.store.getSage(c));
  await s.run('recover-unknown', 'recoverSage', { id: 'x' }, () => s.store.recoverSage('01NOTFOUNDNOTFOUNDNOTFOUND'));
  void b;
}

// ─── counters ───────────────────────────────────────────────────────────────
{
  const s = new Scenario('counters', 'recordInjection / recordUse: JSON counters, updated_at, FTS and HQ churn');
  scenarios.push(s);
  const m = s.label(idOf(await s.run('seed', 'rememberSage', {}, () =>
    s.store.rememberSage({ text: 'Integration tests require the local Postgres container to be running.' }),
  )), 'm')!;
  await s.run('inject-twice-same-call', 'recordInjection', { ids: [m, m], trigger: 'tool' }, () => s.store.recordInjection([m, m], 'tool', 'sess-A'));
  await s.run('inject-again', 'recordInjection', { ids: [m], trigger: 'turn' }, () => s.store.recordInjection([m], 'turn'));
  await s.run('use', 'recordUse', { ids: [m], source: 'reference' }, () => s.store.recordUse([m], 'reference', 'sess-A'));
  await s.run('content-update-keeps-counters', 'updateSage', { id: m, importance: 0.9 }, () => s.store.updateSage(m, { importance: 0.9 }));
  await s.run('get', 'getSage', { id: m }, () => s.store.getSage(m));
}

// ─── candidates ─────────────────────────────────────────────────────────────
{
  const s = new Scenario('candidates', 'createCandidate, list, accept, reject, resolve (delete/archive/keep), stale targetRevision, consolidation');
  scenarios.push(s);
  const rem = async (label: string, input: Row) =>
    s.label(idOf(await s.run(`remember-${label}`, 'rememberSage', input, () => s.store.rememberSage(input))), label)!;
  const t1 = await rem('t1', { text: 'The legacy cron runner is deprecated in favor of the job queue.' });
  const t2 = await rem('t2', { text: 'Old feature flag service lives in services/flags-v1.' });
  const t3 = await rem('t3', { text: 'Staging uses the shared Redis cluster for sessions.' });
  const tp = await rem('tperm', { text: 'Audit logs must be retained for seven years.', persistence: 'permanent' });
  const m1 = await rem('m1', { text: 'Use the job queue for background work in the API service.' });
  const m2 = await rem('m2', { text: 'Background jobs in the API run on the job queue, not cron.' });
  const cc = async (label: string, input: Row) =>
    s.label(idOf(await s.run(`create-${label}`, 'createCandidate', input, () => s.store.createCandidate(input))), label);
  await cc('plain', { text: 'CI caches the bun install directory between jobs.', tags: ['CI'] });
  await cc('plain-duplicate', { text: '  ci caches the BUN install directory between jobs. ' });
  await cc('session', { text: 'Session-specific proposal about local debugging setup.', scope: 'session', ownerSessionId: 'sess-A' });
  await cc('review-delete', { text: 'Review: cron runner note is obsolete.', kind: 'memory_review', targetMemoryId: t1, targetRevision: 1, reviewReason: 'noise', suggestedAction: 'delete' });
  await cc('review-archive', { text: 'Review: flags-v1 note is outdated.', kind: 'memory_review', targetMemoryId: t2, reviewReason: 'outdated', suggestedAction: 'archive' });
  await cc('review-keep', { text: 'Review: redis note still correct.', kind: 'memory_review', targetMemoryId: t3, suggestedAction: 'keep' });
  await cc('review-stale-rev', { text: 'Review: redis note stale revision.', kind: 'memory_review', targetMemoryId: t3, targetRevision: 7, suggestedAction: 'delete' });
  await cc('review-permanent', { text: 'Review: audit retention.', kind: 'memory_review', targetMemoryId: tp, suggestedAction: 'delete' });
  await cc('consolidation', {
    text: 'Background work in the API service runs on the job queue; cron is not used.',
    suggestedAction: 'consolidate',
    supersedesMemoryIds: [m1, m2],
    supersedesRevisions: [1, 1],
  });
  await cc('bad-target-revision', { text: 'Target revision without target id is invalid.', targetRevision: 1 });
  await cc('bad-supersedes', { text: 'Supersedes ids without revisions are invalid.', supersedesMemoryIds: [m1] });
  await cc('secret', { text: 'token = abcdefghijklmnopqrstuvwxyz123456' });
  await s.run('list-pending', 'listCandidates', { includeResolved: false }, () => s.store.listCandidates(false));
  const id = (l: string) => [...s.labels.entries()].find(([, v]) => v === l)?.[0];
  const cand = (l: string) => `candidate_${id(l)}`;
  await s.run('accept-plain', 'acceptCandidate', { id: cand('plain') }, () => s.store.acceptCandidate(cand('plain')));
  await s.run('accept-again-undefined', 'acceptCandidate', { id: cand('plain') }, () => s.store.acceptCandidate(cand('plain')));
  await s.run('accept-review-rejected', 'acceptCandidate', { id: cand('review-delete') }, () => s.store.acceptCandidate(cand('review-delete')));
  await s.run('reject-session', 'rejectCandidate', { id: cand('session'), reason: '  not durable  ' }, () =>
    s.store.rejectCandidate(cand('session'), '  not durable  '),
  );
  await s.run('reject-again-false', 'rejectCandidate', { id: cand('session') }, () => s.store.rejectCandidate(cand('session'), 'x'));
  await s.run('resolve-delete', 'resolveCandidate', { id: cand('review-delete'), decision: 'delete' }, () =>
    s.store.resolveCandidate(cand('review-delete'), 'delete'),
  );
  await s.run('resolve-archive', 'resolveCandidate', { id: cand('review-archive'), decision: 'archive', reason: 'superseded by v2' }, () =>
    s.store.resolveCandidate(cand('review-archive'), 'archive', 'superseded by v2'),
  );
  await s.run('resolve-keep', 'resolveCandidate', { id: cand('review-keep'), decision: 'keep' }, () =>
    s.store.resolveCandidate(cand('review-keep'), 'keep'),
  );
  await s.run('resolve-stale-revision-reverts', 'resolveCandidate', { id: cand('review-stale-rev'), decision: 'delete' }, () =>
    s.store.resolveCandidate(cand('review-stale-rev'), 'delete'),
  );
  await s.run('resolve-permanent-noop', 'resolveCandidate', { id: cand('review-permanent'), decision: 'delete' }, () =>
    s.store.resolveCandidate(cand('review-permanent'), 'delete'),
  );
  await s.run('resolve-already-resolved', 'resolveCandidate', { id: cand('review-keep'), decision: 'delete' }, () =>
    s.store.resolveCandidate(cand('review-keep'), 'delete'),
  );
  await s.run('resolve-unknown', 'resolveCandidate', { id: 'candidate_x', decision: 'keep' }, () => s.store.resolveCandidate('candidate_x', 'keep'));
  const accepted = await s.run('accept-consolidation', 'acceptCandidate', { id: cand('consolidation') }, () => s.store.acceptCandidate(cand('consolidation')));
  s.label(idOf(accepted), 'consolidated');
  await s.run('list-all', 'listCandidates', { includeResolved: true }, () => s.store.listCandidates(true));
}

// ─── hygiene ────────────────────────────────────────────────────────────────
{
  const s = new Scenario('hygiene', 'default hygiene: verification, exact/near dedup, contradictions, session GC, expiry, review candidates, permanent safeguard');
  scenarios.push(s);
  s.file('src/present.ts', 'export const present = true;\n');
  s.file('src/gone.ts', 'temporary\n');
  const rem = async (label: string, input: Row) =>
    s.label(idOf(await s.run(`remember-${label}`, 'rememberSage', input, () => s.store.rememberSage(input))), label)!;
  await rem('anchoredPresent', { text: 'present.ts exports the present flag used by the router.', anchors: [{ type: 'file', path: 'src/present.ts' }] });
  await rem('anchoredGone', { text: 'gone.ts holds the temporary migration helper for v2.', anchors: [{ type: 'file', path: 'src/gone.ts' }] });
  await rem('dupA', { text: 'Logs are shipped to the central collector every minute.', importance: 0.5 });
  await rem('session', { text: 'Session scratch about the local proxy configuration.', scope: 'session', ownerSessionId: 'sess-old' });
  await rem('expiring', { text: 'Temporary freeze on dependency upgrades until the audit completes.', expiresAt: new Date(EPOCH + 3 * 86_400_000).toISOString() });
  await rem('permanent', { text: 'Customer data never leaves the EU region.', persistence: 'permanent', expiresAt: new Date(EPOCH + 86_400_000).toISOString() });
  await rem('lowConfidence', { text: 'The analytics job might use a separate replica database.', confidence: 0.3 });
  await rem('polarityA', { text: 'The cache layer is invalidated on every deploy of the api service.' });
  await rem('polarityB', { text: 'The cache layer is not invalidated on every deploy of the api service.' });
  // Exact duplicate inserted through raw SQL so remember's merge does not fold it.
  {
    const db = new DatabaseSync(s.dbFile());
    const row = db.prepare('SELECT data FROM memories WHERE id = ?').get([...s.labels.entries()].find(([, l]) => l === 'dupA')![0]) as { data: string };
    const copy = { ...JSON.parse(row.data), id: '01DUPLICATECOPY00000000000B', importance: 0.7, createdAt: new Date(clock).toISOString() };
    db.prepare(
      `INSERT INTO memories (id, data, status, kind, scope, legacy_scope, importance, confidence, freshness, updated_at, created_at, audience, tags, owner_session_id, canonical_text)
       VALUES (?, ?, 'active', ?, ?, 'project-memory', ?, ?, ?, ?, ?, NULL, ?, NULL, ?)`,
    ).run(copy.id, JSON.stringify(copy), copy.kind, copy.scope, copy.importance, copy.confidence, copy.freshness, copy.updatedAt, copy.createdAt, JSON.stringify(copy.tags), 'logs are shipped to the central collector every minute.');
    db.close();
    s.label('01DUPLICATECOPY00000000000B', 'dupB');
  }
  rmSync(join(s.root, 'src/gone.ts'));
  tick(8 * 86_400); // past session retention (7d), expiresAt, but under 30d review thresholds
  await s.run('hygiene-default', 'hygiene', {}, () => s.store.hygiene());
  tick(31 * 86_400);
  await s.run('hygiene-after-31-days', 'hygiene', {}, () => s.store.hygiene());
  await s.run('hygiene-purge', 'hygiene', { purgeDeletedAfterDays: 1 }, () => s.store.hygiene({ purgeDeletedAfterDays: 1 }));
  s.file('src/gone.ts', 'restored\n');
  await s.run('hygiene-reactivates-verification-stale', 'hygiene', { nearDedup: false }, () => s.store.hygiene({ nearDedup: false }));
}

// ─── graph / retrieval ──────────────────────────────────────────────────────
{
  const s = new Scenario('graph-retrieval', 'findRelatedSage, graphFor, traverseGraph, retrieveForPath expansion, listSagePage cursor walk, multi-term search rerank, getStats');
  scenarios.push(s);
  s.file('src/server/http.ts', 'export function startServer() {}\n');
  s.file('src/server/routes.ts', 'export const routes = [];\n');
  s.file('src/lib/util.ts', 'export const u = 1;\n');
  const rem = async (label: string, input: Row) =>
    s.label(idOf(await s.run(`remember-${label}`, 'rememberSage', input, () => s.store.rememberSage(input), { state: false })), label)!;
  const http = await rem('http', { text: 'startServer binds the port before plugins load.', kind: 'symbol_note', anchors: [{ type: 'symbol', path: 'src/server/http.ts', symbol: 'startServer' }], tags: ['server'] });
  await rem('routes', { text: 'Routes are registered in src/server/routes.ts in declaration order.', anchors: [{ type: 'file', path: 'src/server/routes.ts' }], tags: ['server'] });
  await rem('serverDir', { text: 'Everything under src/server is owned by the platform team.', anchors: [{ type: 'directory', path: 'src/server' }] });
  await rem('util', { text: 'util.ts helpers must stay free of side effects.', anchors: [{ type: 'file', path: 'src/lib/util.ts' }] });
  await rem('cmd', { text: 'Run bun test --watch while editing the server module.', anchors: [{ type: 'command', command: 'bun test --watch' }] });
  await rem('reviewer', { text: 'Reviewers check error handling paths in the server.', kind: 'role_operational', anchors: [{ type: 'agent', role: 'reviewer' }], audience: { roles: ['reviewer'] } });
  await rem('successor', { text: 'startServer now binds the port after plugins load to allow port overrides.', kind: 'symbol_note', anchors: [{ type: 'symbol', path: 'src/server/http.ts', symbol: 'startServer' }], supersedes: [http] });
  for (let i = 0; i < 6; i++) await rem(`filler${i}`, { text: `Filler fact number ${i} about the deployment pipeline and server rollout timing.`, importance: 0.3 + i * 0.1 });
  await s.run('state', 'dump', {}, () => undefined);
  await s.run('retrieveForPath-file', 'retrieveForPath', { paths: ['src/server/http.ts'] }, () => s.store.retrieveForPath(['src/server/http.ts']), { state: false });
  await s.run('retrieveForPath-no-ancestors', 'retrieveForPath', { paths: ['src/server/http.ts'], includeAncestors: false }, () =>
    s.store.retrieveForPath(['src/server/http.ts'], { includeAncestors: false }), { state: false });
  await s.run('retrieveForPath-dir', 'retrieveForPath', { paths: ['src/server'] }, () => s.store.retrieveForPath(['src/server']), { state: false });
  await s.run('retrieveForPath-absolute', 'retrieveForPath', { paths: ['<root>/src/lib/util.ts'] }, () => s.store.retrieveForPath([join(s.root, 'src/lib/util.ts')]), { state: false });
  await s.run('retrieveForPath-no-audience', 'retrieveForPath', { paths: ['src/server/http.ts'], includeAudienceScoped: false }, () =>
    s.store.retrieveForPath(['src/server/http.ts'], { includeAudienceScoped: false }), { state: false });
  await s.run('findRelatedSage', 'findRelatedSage', { ids: [http] }, () => s.store.findRelatedSage([http]), { state: false });
  await s.run('findMemoriesForFile', 'findMemoriesForFile', { path: 'src/server/http.ts' }, () => s.store.findMemoriesForFile('src/server/http.ts'), { state: false });
  await s.run('graphFor-file', 'graphFor', { query: 'src/server/http.ts' }, () => s.store.graphFor('src/server/http.ts'), { state: false });
  await s.run('graphFor-memory', 'graphFor', { query: http }, () => s.store.graphFor(http), { state: false });
  await s.run('traverseGraph', 'traverseGraph', { from: `mem:${http}` }, () => s.store.traverseGraph([`mem:${http}`]), { state: false });
  await s.run('retrieveForAudience', 'retrieveForAudience', { role: 'reviewer' }, () => s.store.retrieveForAudience({ role: 'reviewer' }), { state: false });
  for (const q of ['server port plugins', 'deployment pipeline rollout', 'startServer binds', 'filler server', 'bun test watch']) {
    await s.run(`searchSage(${q})`, 'searchSage', { query: q }, () => s.store.searchSage(q), { state: false });
    await s.run(`searchSage(${q}, semanticRerank=false)`, 'searchSage', { query: q, semanticRerank: false }, () => s.store.searchSage(q, { semanticRerank: false }), { state: false });
  }
  await s.run('searchSageWithBreakdown', 'searchSageWithBreakdown', { query: 'server port plugins' }, () => s.store.searchSageWithBreakdown('server port plugins'), { state: false });
  // Rerank internals on fixed inputs.
  const texts = ['startServer binds the port before plugins load.', 'Filler fact about the deployment pipeline.', 'Routes are registered in order.'];
  s.note(
    'hybridRerank-pure',
    ['server port plugins', 'deployment', 'ab'].map((q) => ({
      query: q,
      order: rerank.hybridRerankMemories(q, texts.map((text, i) => ({ id: `t${i}`, text }) as never)).map((m: { id: string }) => m.id),
    })),
  );
  let cursor: string | undefined;
  for (let page = 0; page < 6; page++) {
    const res = (await s.run(`listSagePage-limit3-page${page}`, 'listSagePage', { limit: 3, cursor }, () => s.store.listSagePage({ limit: 3, cursor }), { state: false })) as
      | { nextCursor: string | null }
      | undefined;
    cursor = res?.nextCursor ?? undefined;
    if (!cursor) break;
  }
  await s.run('listSagePage-query', 'listSagePage', { query: 'SERVER' }, () => s.store.listSagePage({ query: 'SERVER' }), { state: false });
  await s.run('listSagePage-bad-cursor', 'listSagePage', { cursor: 'not-a-cursor' }, () => s.store.listSagePage({ cursor: 'not-a-cursor' }), { state: false });
  await s.run('getStats', 'getStats', {}, () => s.store.getStats(), { state: false });
}

// ─── Write ──────────────────────────────────────────────────────────────────
mkdirSync(outDir, { recursive: true });
const index: Array<{ scenario: string; description: string; steps: number; errors: number }> = [];
for (const s of scenarios) {
  await s.store.drainMutations?.();
  writeFileSync(join(outDir, `${s.name}.json`), s.serialize());
  index.push({
    scenario: s.name,
    description: s.description,
    steps: s.steps.length,
    errors: s.steps.filter((st) => st.error).length,
  });
  s.close();
}
writeFileSync(
  join(outDir, 'index.json'),
  `${JSON.stringify({ baseline: baseline.commit, generator: 'scripts/oracle/build-mutation-fixtures.ts', scenarios: index, warnings: warnings.length }, null, 2)}\n`,
);
console.warn = origWarn;
for (const i of index) console.error(`${i.scenario}: ${i.steps} steps, ${i.errors} recorded errors`);
