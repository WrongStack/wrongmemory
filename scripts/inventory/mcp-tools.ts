/**
 * WM-006: generate the actual SAGE MCP tool inventory from the baseline's
 * real tool factory (`packages/sage/src/tools/memory-tools.ts`) and the real
 * sage-mcp exposure policy (`packages/sage-mcp/src/policy.ts`), instead of
 * copying README tool counts.
 *
 * The clean oracle archive does not include `packages/sage-mcp`, so
 * policy.ts is read from the baseline commit with `git show` (read-only) and
 * written to a generated directory beside the oracle; it imports only types.
 *
 * Output: contracts/inventory/mcp-tools.json
 *
 * Usage: bun scripts/inventory/mcp-tools.ts --wrongstack <clean-oracle-dir> --git <wrongstack-checkout>
 */
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

const wrongstack = arg('--wrongstack');
const gitRepo = arg('--git');
if (!wrongstack || !gitRepo) {
  console.error('usage: bun scripts/inventory/mcp-tools.ts --wrongstack <clean-oracle-dir> --git <wrongstack-checkout>');
  process.exit(2);
}
const repo = resolve(import.meta.dir, '..', '..');
const baseline = JSON.parse(readFileSync(join(repo, 'contracts/baseline/wrongstack-baseline.json'), 'utf8')) as {
  commit: string;
};
const marker = JSON.parse(readFileSync(join(resolve(wrongstack), '.wrongmemory-oracle.json'), 'utf8')) as { commit: string };
if (marker.commit !== baseline.commit) {
  console.error(`oracle commit ${marker.commit} does not match baseline ${baseline.commit}`);
  process.exit(1);
}

const genDir = join(resolve(wrongstack), '.wrongmemory-generated', 'sage-mcp');
mkdirSync(genDir, { recursive: true });
const policySource = execFileSync('git', ['-C', resolve(gitRepo), 'show', `${baseline.commit}:packages/sage-mcp/src/policy.ts`], {
  encoding: 'utf8',
});
const policyPath = join(genDir, 'policy.ts');
writeFileSync(policyPath, policySource);

const sageSrc = join(resolve(wrongstack), 'packages/sage/src');
const { SqliteSageStore } = await import(join(sageSrc, 'sqlite-store.ts'));
const { createSageTools } = await import(join(sageSrc, 'tools/memory-tools.ts'));
const policy = await import(policyPath);

const tmpRoot = join(resolve(wrongstack), '.wrongmemory-generated', 'mcp-tools-store');
rmSync(tmpRoot, { recursive: true, force: true });
mkdirSync(tmpRoot, { recursive: true });
const store = new SqliteSageStore({ projectRoot: tmpRoot });
type ToolLike = {
  name: string;
  description?: string;
  inputSchema?: unknown;
  permission?: string;
  riskTier?: string;
  mutating?: boolean;
  readOnly?: boolean;
  validate?: (input: Record<string, unknown>) => string[];
  [k: string]: unknown;
};
const tools = createSageTools(store) as ToolLike[];

const modes = {
  'read-only': {},
  'read-propose': { proposals: true, origin: 'external' },
  writable: { writable: true },
} as const;
const exposure: Record<string, string[]> = {};
const narrowed: Record<string, unknown> = {};
for (const [mode, opts] of Object.entries(modes)) {
  const allowed = policy.selectAllowedTools(tools, opts) as Array<{ name: string; tool: ToolLike }>;
  exposure[mode] = allowed.map((a) => a.name);
  for (const a of allowed) {
    const original = tools.find((t) => t.name === a.name);
    if (original && a.tool !== original) {
      narrowed[`${mode}:${a.name}`] = {
        description: a.tool.description,
        inputSchema: a.tool.inputSchema,
        rejects: {
          accept: a.tool.validate?.({ action: 'accept', candidateId: 'x' }),
          sessionScopePropose: a.tool.validate?.({ action: 'propose', text: 'A durable fact for review.', scope: 'session' }),
        },
      };
    }
  }
}

const inventory = {
  schema: 'wrongmemory/mcp-tool-inventory@1',
  source: {
    commit: baseline.commit,
    factory: 'packages/sage/src/tools/memory-tools.ts#createSageTools',
    policy: 'packages/sage-mcp/src/policy.ts#selectAllowedTools',
    generatedBy: 'scripts/inventory/mcp-tools.ts',
  },
  policyRule:
    "deny -> never; memory_candidates with proposals && !writable -> narrowed to list/propose (session scope refused); riskTier destructive -> never; permission auto -> exposed if riskTier safe or writable; permission confirm -> only writable and riskTier standard|safe",
  exposure,
  narrowed,
  tools: tools.map((t) => ({
    name: t.name,
    description: t.description,
    permission: t.permission,
    riskTier: t.riskTier,
    mutating: t.mutating,
    readOnly: t.readOnly,
    inputSchema: t.inputSchema,
    otherKeys: Object.keys(t)
      .filter((k) => !['name', 'description', 'inputSchema', 'permission', 'riskTier', 'mutating', 'readOnly', 'execute', 'validate'].includes(k))
      .sort(),
  })),
};
store.close();
mkdirSync(join(repo, 'contracts/inventory'), { recursive: true });
writeFileSync(join(repo, 'contracts/inventory/mcp-tools.json'), `${JSON.stringify(inventory, null, 2)}\n`);
console.error(`${tools.length} tools; read-only ${exposure['read-only']!.length}, read-propose ${exposure['read-propose']!.length}, writable ${exposure.writable!.length}`);
if (!existsSync(join(repo, 'contracts/inventory/mcp-tools.json'))) process.exit(1);
