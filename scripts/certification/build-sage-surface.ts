/**
 * WM-511: generate the SAGE surface certification matrix.
 *
 *   bun run certification:surface -- --wrongstack <baseline checkout> [--adapter <WrongStack worktree>]
 *
 * Reads the baseline's operation list (`SAGE_DISPATCH_FIELD_SPECS` in
 * packages/sage/src/project-server-protocol.ts) and maps every operation to the
 * WrongMemory daemon methods that serve it and the tests that are its evidence.
 * The mapping below is the reviewed input; the JSON in
 * contracts/certification/sage-surface.json is generated from it and must not
 * be hand-edited.
 *
 * The script refuses to write when:
 *  - the baseline lists an operation the mapping does not cover, or the other way round;
 *  - a mapped method is not a WrongMemory method constant;
 *  - a Rust evidence test does not exist in crates/*\/tests or crates/*\/src;
 *  - with --adapter, an adapter evidence test title does not exist in the worktree.
 *
 * Status values: COVERED (served and tested), UNSUPPORTED (refused on purpose,
 * reason given). Nothing here claims client certification or a live host run.
 */
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';

type Status = 'COVERED' | 'UNSUPPORTED';
interface Entry {
  methods: string[];
  rust: string[];
  adapter: string[];
  status?: Status;
  note?: string;
}

const ADAPTER_TESTS = 'packages/sage/tests/wrongmemory-port.integration.test.ts';

// Reviewed mapping: baseline operation -> daemon methods and evidence.
const MAPPING: Record<string, Entry> = {
  getHqSyncVersion: {
    methods: ['memory.hqSyncVersion'],
    rust: ['hq_sync_scenario_matches_oracle', 'hq_sync_is_host_only_and_never_projects_sessions'],
    adapter: ['HQ sync runs through the daemon in wrongmemory mode (WM-408)'],
  },
  listHqSync: {
    methods: ['memory.hqSyncList'],
    rust: ['hq_sync_scenario_matches_oracle', 'hq_sync_is_host_only_and_never_projects_sessions'],
    adapter: ['HQ sync runs through the daemon in wrongmemory mode (WM-408)'],
  },
  applyHqSync: {
    methods: ['memory.hqSyncApply'],
    rust: ['hq_sync_scenario_matches_oracle', 'hq_sync_is_host_only_and_never_projects_sessions'],
    adapter: ['HQ sync runs through the daemon in wrongmemory mode (WM-408)'],
  },
  ping: {
    methods: ['daemon.health'],
    rust: ['grants_projects_sessions_and_receipts_hold_across_restart'],
    adapter: ['registers a managed project and reports ready'],
    note: 'Served as daemon health plus project admission; the SAGE project-server status shape is not reproduced.',
  },
  readAll: {
    methods: ['memory.listPage'],
    rust: [],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
    note: 'Composed in the adapter with the baseline formatter (readAllSqliteMemory).',
  },
  read: {
    methods: ['memory.listPage'],
    rust: [],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
    note: 'Composed in the adapter with the baseline formatter (readSqliteMemory).',
  },
  remember: {
    methods: ['memory.remember'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
  },
  forget: {
    methods: ['memory.legacyForget'],
    rust: ['legacy_bulk_scenario_matches_oracle', 'legacy_bulk_mutations_are_curated_and_session_safe'],
    adapter: ['legacy forget / consolidate / clear run on the daemon'],
  },
  consolidate: {
    methods: ['memory.legacyConsolidate'],
    rust: ['legacy_bulk_scenario_matches_oracle', 'legacy_bulk_mutations_are_curated_and_session_safe'],
    adapter: ['legacy forget / consolidate / clear run on the daemon'],
  },
  clear: {
    methods: ['memory.legacyClear'],
    rust: ['legacy_bulk_scenario_matches_oracle', 'legacy_bulk_mutations_are_curated_and_session_safe'],
    adapter: ['legacy forget / consolidate / clear run on the daemon'],
  },
  list: {
    methods: ['memory.listPage'],
    rust: ['graph_retrieval_reads_match_oracle'],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
  },
  search: {
    methods: ['memory.search'],
    rust: ['search_matches_oracle'],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
  },
  findRelated: {
    methods: ['memory.search'],
    rust: ['search_matches_oracle'],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
    note: 'As in the baseline service capability, legacy findRelated is the legacy search.',
  },
  scoreRelevant: {
    methods: ['memory.search'],
    rust: ['search_matches_oracle'],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
    note: 'Composed in the adapter like the in-process SAGE port (search ranked by position).',
  },
  stats: {
    methods: ['memory.stats'],
    rust: ['graph_retrieval_reads_match_oracle'],
    adapter: ['createCandidate -> acceptCandidate -> retrieveForPath'],
  },
  listSage: {
    methods: ['memory.listPage'],
    rust: ['graph_retrieval_reads_match_oracle'],
    adapter: ['legacy forget / consolidate / clear run on the daemon'],
  },
  listSagePage: {
    methods: ['memory.listPage'],
    rust: ['graph_retrieval_reads_match_oracle'],
    adapter: ['serves legacy MemoryStore reads from SAGE'],
  },
  getSage: {
    methods: ['memory.get'],
    rust: ['exact_id_reads_match_oracle_records', 'grants_cannot_cross_projects_or_sessions'],
    adapter: ['remember -> search -> update with expectedRevision conflict'],
  },
  rememberSage: {
    methods: ['memory.remember'],
    rust: ['core_mutation_scenarios_match_oracle', 'crash_after_commit_before_reply_replays_idempotent_remember'],
    adapter: ['remember -> search -> update with expectedRevision conflict'],
  },
  updateSage: {
    methods: ['memory.update', 'memory.feedback'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: ['remember -> search -> update with expectedRevision conflict'],
    note: 'Deliberate deviation: an ordinary update naming scope/ownerSessionId is refused instead of silently ignored.',
  },
  deleteSage: {
    methods: ['memory.delete'],
    rust: ['delete_recover_scenario_matches_oracle'],
    adapter: [],
    note: 'Destructive deletes need the curator grant; the client force flag alone never authorizes.',
  },
  retrieveForPath: {
    methods: ['memory.forPath'],
    rust: ['path_retrieval_matches_oracle_sets', 'graph_retrieval_reads_match_oracle'],
    adapter: ['createCandidate -> acceptCandidate -> retrieveForPath'],
  },
  searchSage: {
    methods: ['memory.search'],
    rust: ['search_matches_oracle'],
    adapter: ['fuses a caller-owned vector channel once, never leaking hidden or stale ids (WM-405)'],
  },
  searchSageWithBreakdown: {
    methods: ['memory.search'],
    rust: ['search_matches_oracle'],
    adapter: ['fuses a caller-owned vector channel once, never leaking hidden or stale ids (WM-405)'],
  },
  unifiedSearch: {
    methods: ['memory.unifiedSearch'],
    rust: ['unified_search_matches_oracle'],
    adapter: ['unifiedSearchService walks keyset cursors on the daemon'],
  },
  findRelatedSage: {
    methods: ['memory.related'],
    rust: ['graph_retrieval_reads_match_oracle'],
    adapter: [],
  },
  recordInjection: {
    methods: ['memory.recordUsage'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: [],
  },
  recordUse: {
    methods: ['memory.recordUsage'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: [],
  },
  retrieveForAudience: {
    methods: ['memory.forAudience'],
    rust: ['audience_retrieval_matches_oracle', 'audience_context_is_bounded_before_it_reaches_the_audit_log'],
    adapter: ['retrieveForAudience matches roles case-insensitively and reports truncation'],
  },
  graphFor: {
    methods: ['memory.graph'],
    rust: ['graph_retrieval_reads_match_oracle'],
    adapter: [],
  },
  verify: {
    methods: ['memory.verify'],
    rust: ['verify_scenario_matches_oracle', 'verify_is_curated_and_session_safe'],
    adapter: ['verify and verifyForPaths reconcile anchors on the daemon'],
  },
  verifyForPaths: {
    methods: ['memory.verify'],
    rust: ['verify_scenario_matches_oracle', 'verify_is_curated_and_session_safe'],
    adapter: ['verify and verifyForPaths reconcile anchors on the daemon'],
  },
  hygiene: {
    methods: ['memory.hygiene'],
    rust: ['hygiene_scenario_matches_oracle', 'hygiene_never_reactivates_manual_stale_and_dry_run_writes_nothing'],
    adapter: ['maintenance schedule hands hygiene to the daemon scheduler (WM-404)'],
  },
  listCandidates: {
    methods: ['candidate.list'],
    rust: ['core_mutation_scenarios_match_oracle', 'hidden_candidates_do_not_starve_visible_ones'],
    adapter: ['createCandidate -> acceptCandidate -> retrieveForPath'],
  },
  createCandidate: {
    methods: ['candidate.propose'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: ['consolidation proposals supersede members; a stale review stays pending (WM-511)'],
    note: 'Deliberate deviation: consolidation members must be visible to the proposer.',
  },
  resolveCandidate: {
    methods: ['candidate.resolve'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: ['consolidation proposals supersede members; a stale review stays pending (WM-511)'],
  },
  acceptCandidate: {
    methods: ['candidate.accept'],
    rust: ['core_mutation_scenarios_match_oracle', 'crash_during_candidate_accept_is_accepted_exactly_once'],
    adapter: ['createCandidate -> acceptCandidate -> retrieveForPath'],
  },
  rejectCandidate: {
    methods: ['candidate.reject'],
    rust: ['core_mutation_scenarios_match_oracle'],
    adapter: ['consolidation proposals supersede members; a stale review stays pending (WM-511)'],
  },
  recoverSage: {
    methods: ['memory.recover'],
    rust: ['delete_recover_scenario_matches_oracle'],
    adapter: [],
  },
  backfillRecoverable: {
    methods: ['memory.backfill'],
    rust: ['delete_recover_scenario_matches_oracle', 'backfill_skip_reasons_follow_baseline_order'],
    adapter: [],
  },
  findMemoriesForFile: {
    methods: ['memory.forFile'],
    rust: ['for_file_review_hints_respect_sessions'],
    adapter: ['findMemoriesForFile groups file and symbol matches'],
  },
  readAudit: {
    methods: ['memory.audit'],
    rust: ['doctor_scenario_matches_oracle', 'doctor_and_audit_are_host_only'],
    adapter: ['doctor and readAudit run on the daemon (WM-406)'],
  },
  importLegacyFiles: {
    methods: ['memory.importLegacy'],
    rust: ['legacy_import_only_reads_project_files'],
    adapter: ['importLegacy imports Markdown bullets and refuses files outside the project'],
  },
  consolidateSession: {
    methods: ['memory.search', 'candidate.propose', 'candidate.accept'],
    rust: [],
    adapter: ['consolidateSession turns session facts into candidates (WM-511 surface)'],
    note: 'Composed in the adapter with the baseline policy (shared/session-consolidation).',
  },
  maintenance: {
    methods: ['maintenance.status', 'maintenance.configure', 'maintenance.run'],
    rust: ['scheduler_runs_due_jobs_and_recovers_interrupted_ones', 'maintenance_pass_self_heals_structural_defects'],
    adapter: ['maintenance schedule hands hygiene to the daemon scheduler (WM-404)'],
  },
  doctor: {
    methods: ['memory.doctor'],
    rust: ['doctor_scenario_matches_oracle', 'doctor_and_audit_are_host_only'],
    adapter: ['doctor and readAudit run on the daemon (WM-406)'],
  },
};

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function walk(dir: string, out: string[] = []): string[] {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else if (e.name.endsWith('.rs')) out.push(p);
  }
  return out;
}

const repo = path.resolve(import.meta.dirname, '..', '..');
const wrongstack = arg('--wrongstack');
if (!wrongstack) throw new Error('--wrongstack <baseline checkout> is required');
const adapter = arg('--adapter');

const protocolPath = path.join(wrongstack, 'packages/sage/src/project-server-protocol.ts');
const protocol = readFileSync(protocolPath, 'utf8');
const block = protocol.match(
  /export const SAGE_DISPATCH_FIELD_SPECS = \{([\s\S]*?)\} as const satisfies/,
)?.[1];
if (!block) throw new Error('SAGE_DISPATCH_FIELD_SPECS not found in the baseline protocol');
const ops = [...block.matchAll(/^ {2}([A-Za-z]+):/gm)].map((m) => m[1] as string);

const problems: string[] = [];
const mapped = Object.keys(MAPPING);
for (const op of ops) if (!MAPPING[op]) problems.push(`baseline op ${op} is not mapped`);
for (const op of mapped) if (!ops.includes(op)) problems.push(`mapped op ${op} is not a baseline op`);

const contracts = readFileSync(path.join(repo, 'crates/wrongmemory-contracts/src/lib.rs'), 'utf8');
const methods = new Set(
  [...contracts.matchAll(/pub const [A-Z_]+: &str = "([a-z]+\.[A-Za-z]+)"/g)].map((m) => m[1]),
);
const rustSources = walk(path.join(repo, 'crates'))
  .map((f) => readFileSync(f, 'utf8'))
  .join('\n');
const adapterSource =
  adapter && existsSync(path.join(adapter, ADAPTER_TESTS))
    ? readFileSync(path.join(adapter, ADAPTER_TESTS), 'utf8')
    : undefined;
if (adapter && !adapterSource) problems.push(`adapter tests not found under ${adapter}`);

for (const [op, e] of Object.entries(MAPPING)) {
  for (const m of e.methods) if (!methods.has(m)) problems.push(`${op}: unknown method ${m}`);
  for (const t of e.rust)
    if (!new RegExp(`fn ${t}\\(`).test(rustSources)) problems.push(`${op}: no Rust test ${t}`);
  if (adapterSource)
    for (const t of e.adapter)
      if (!adapterSource.includes(`'${t}'`)) problems.push(`${op}: no adapter test '${t}'`);
  if (e.status !== 'UNSUPPORTED' && e.rust.length + e.adapter.length === 0)
    problems.push(`${op}: COVERED without evidence`);
}
if (problems.length) {
  console.error(problems.join('\n'));
  process.exit(1);
}

const baselineCommit = (() => {
  const head = path.join(wrongstack, '.git', 'HEAD');
  if (existsSync(head)) return readFileSync(head, 'utf8').trim();
  const name = path.basename(path.resolve(wrongstack));
  return name.match(/[0-9a-f]{12}$/)?.[0] ?? 'unknown';
})();

const matrix = {
  generatedBy: 'scripts/certification/build-sage-surface.ts',
  baseline: {
    commit: baselineCommit,
    protocol: 'packages/sage/src/project-server-protocol.ts',
    protocolSha256: createHash('sha256').update(protocol).digest('hex'),
    operations: ops.length,
  },
  adapterEvidence: { file: ADAPTER_TESTS, checked: Boolean(adapterSource) },
  summary: {
    covered: ops.filter((o) => MAPPING[o]?.status !== 'UNSUPPORTED').length,
    unsupported: ops.filter((o) => MAPPING[o]?.status === 'UNSUPPORTED').length,
  },
  operations: ops.map((op) => {
    const e = MAPPING[op] as Entry;
    return {
      op,
      status: e.status ?? 'COVERED',
      methods: e.methods,
      rustEvidence: e.rust,
      adapterEvidence: e.adapter,
      ...(e.note ? { note: e.note } : {}),
    };
  }),
};

const outDir = path.join(repo, 'contracts', 'certification');
mkdirSync(outDir, { recursive: true });
writeFileSync(path.join(outDir, 'sage-surface.json'), `${JSON.stringify(matrix, null, 2)}\n`);
console.log(
  `sage-surface.json: ${matrix.summary.covered} covered, ${matrix.summary.unsupported} unsupported of ${ops.length}`,
);
