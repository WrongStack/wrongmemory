/**
 * WM-000: capture an immutable WrongStack source baseline for SAGE.
 *
 * Records the WrongStack HEAD commit, the SHA-256 of every contract file's
 * HEAD blob, and whether the working tree diverges from HEAD for that file.
 * Fixtures must only be regenerated from a clean oracle archived at this
 * commit (prepare-clean-wrongstack.ts), never from dirty working-tree code.
 *
 * Also records whether the SAGE contract scope changed between the commit the
 * plan was researched at (`--plan-commit`, default ad9f79d) and this baseline.
 *
 * Usage: bun scripts/baseline/export.ts --wrongstack <path> [--out <file>] [--plan-commit <sha>]
 */
import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';

const CONTRACT_PATH_PREFIXES = [
  '.bun-version',
  'packages/sage/package.json',
  'packages/sage/src/',
  'packages/sage-mcp/package.json',
  'packages/sage-mcp/src/',
  'packages/persistence/src/sqlite-text-index.ts',
  'packages/persistence/src/sqlite-runtime.ts',
  'packages/vector-memory/package.json',
  'packages/vector-memory/src/',
  'packages/runtime/src/container.ts',
  'packages/cli/src/sage-connect.ts',
  'packages/cli/src/wiring/sage.ts',
  'packages/mcp/package.json',
  'packages/fleet/src/host-memory-companion.ts',
  'packages/webui-server/src/server/memory-handlers.ts',
  'packages/tools/src/codebase-index/ecosystem-bridge.ts',
  'packages/core/src/utils/ulid.ts',
  'docs/plans/unified-sage-search-backend-contract.md',
];

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function git(cwd: string, args: string[], allowFail = false): Buffer {
  const r = Bun.spawnSync(['git', ...args], { cwd, stdout: 'pipe', stderr: 'pipe' });
  if (r.exitCode !== 0 && !allowFail) {
    throw new Error(`git ${args.join(' ')} failed: ${r.stderr.toString()}`);
  }
  return Buffer.from(r.stdout);
}

const wrongstack = arg('--wrongstack');
if (!wrongstack) {
  console.error('usage: bun scripts/baseline/export.ts --wrongstack <path> [--out <file>] [--plan-commit <sha>]');
  process.exit(2);
}
const out = resolve(arg('--out') ?? 'contracts/baseline/wrongstack-baseline.json');
const planCommitArg = arg('--plan-commit') ?? 'ad9f79dbc2aad903b0348e65abd7449273ec1a1c';

const head = git(wrongstack, ['rev-parse', 'HEAD']).toString().trim();
const inScope = (p: string) => CONTRACT_PATH_PREFIXES.some((prefix) => p === prefix || p.startsWith(prefix));
const tracked = git(wrongstack, ['ls-tree', '-r', '--name-only', 'HEAD'])
  .toString()
  .split('\n')
  .filter(Boolean)
  .filter(inScope)
  .sort();

const dirtyAll = git(wrongstack, ['status', '--porcelain', '--untracked-files=no'])
  .toString()
  .split('\n')
  .filter(Boolean)
  .map((line) => line.slice(3).replace(/^"|"$/g, ''));
const dirtySet = new Set(dirtyAll);

const files = tracked.map((path) => {
  const blob = git(wrongstack, ['show', `HEAD:${path}`]);
  return {
    path,
    sha256: createHash('sha256').update(blob).digest('hex'),
    bytes: blob.length,
    workingTreeDirty: dirtySet.has(path),
  };
});

const planCommit = git(wrongstack, ['rev-parse', `${planCommitArg}^{commit}`], true).toString().trim();
const scopeDiff = planCommit
  ? git(wrongstack, ['diff', '--name-only', planCommit, head])
      .toString()
      .split('\n')
      .filter(Boolean)
      .filter(inScope)
  : null;

const manifest = {
  schema: 'wrongmemory.baseline/1',
  source: 'https://github.com/WrongStack/WrongStack',
  commit: head,
  capturedWith: { bun: Bun.version },
  scope: CONTRACT_PATH_PREFIXES,
  planResearchCommit: planCommit || planCommitArg,
  contractFilesChangedSincePlanCommit: scopeDiff,
  repositoryDirtyFileCount: dirtyAll.length,
  contractFilesDirty: files.filter((f) => f.workingTreeDirty).map((f) => f.path),
  files,
};

mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify(manifest, null, 2)}\n`);
console.error(
  `baseline ${head}: ${files.length} contract files, ${manifest.contractFilesDirty.length} dirty in working tree, ` +
    `${scopeDiff?.length ?? 'unknown'} changed since ${planCommitArg.slice(0, 7)} -> ${out}`,
);
