/**
 * WM-011 / WM-704: third-party license inventory for the Rust workspace.
 *
 * Reads `cargo metadata --locked` (every package in the resolved graph, all
 * platforms) and writes `contracts/inventory/licenses.json`. Exits non-zero
 * when a package has no license or no permissive alternative: an SPDX
 * expression passes when at least one OR-alternative consists only of
 * allowed licenses (`A AND B` needs both).
 *
 *   bun scripts/inventory/licenses.ts           # write + check
 *   bun scripts/inventory/licenses.ts --check   # check only
 */
import { writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const ALLOWED = new Set([
  'MIT',
  'Apache-2.0',
  'Apache-2.0 WITH LLVM-exception',
  'BSD-2-Clause',
  'BSD-3-Clause',
  'ISC',
  'Zlib',
  'Unicode-3.0',
  'Unicode-DFS-2016',
  'Unlicense',
  '0BSD',
  'CC0-1.0',
  'BSL-1.0',
]);

const repo = resolve(import.meta.dir, '..', '..');
const checkOnly = process.argv.includes('--check');

const proc = Bun.spawnSync(['cargo', 'metadata', '--format-version', '1', '--locked'], {
  cwd: repo,
  stdout: 'pipe',
  stderr: 'inherit',
});
if (proc.exitCode !== 0) {
  console.error('cargo metadata failed');
  process.exit(1);
}
const meta = JSON.parse(proc.stdout.toString()) as {
  packages: Array<{ id: string; name: string; version: string; license: string | null; source: string | null }>;
  workspace_members: string[];
};
const workspace = new Set(meta.workspace_members);

/** Strip one pair of enclosing parentheses when they wrap the whole string. */
function unwrap(expr: string): string {
  let e = expr.trim();
  while (e.startsWith('(') && e.endsWith(')')) {
    let depth = 0;
    let wraps = true;
    for (let i = 0; i < e.length; i++) {
      if (e[i] === '(') depth++;
      else if (e[i] === ')') depth--;
      if (depth === 0 && i < e.length - 1) {
        wraps = false;
        break;
      }
    }
    if (!wraps) break;
    e = e.slice(1, -1).trim();
  }
  return e;
}

/** Split on a top-level binary operator (OR / AND). */
function splitTop(expr: string, op: 'OR' | 'AND'): string[] {
  const parts: string[] = [];
  let depth = 0;
  let start = 0;
  const tokens = expr;
  for (let i = 0; i < tokens.length; i++) {
    const ch = tokens[i];
    if (ch === '(') depth++;
    else if (ch === ')') depth--;
    else if (depth === 0 && tokens.startsWith(` ${op} `, i)) {
      parts.push(tokens.slice(start, i));
      start = i + op.length + 2;
      i = start - 1;
    }
  }
  parts.push(tokens.slice(start));
  return parts.map(unwrap);
}

function permissive(expr: string): boolean {
  const e = unwrap(expr.replace(/\//g, ' OR '));
  const alternatives = splitTop(e, 'OR');
  if (alternatives.length > 1) return alternatives.some(permissive);
  const conjuncts = splitTop(e, 'AND');
  if (conjuncts.length > 1) return conjuncts.every(permissive);
  return ALLOWED.has(e);
}

const rows = meta.packages
  .filter((p) => !workspace.has(p.id))
  .map((p) => ({
    name: p.name,
    version: p.version,
    license: p.license,
    source: p.source?.startsWith('registry+') ? 'crates.io' : p.source,
    permissive: p.license !== null && permissive(p.license),
  }))
  .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const failing = rows.filter((r) => !r.permissive);
const out = {
  generator: 'scripts/inventory/licenses.ts',
  allowed: [...ALLOWED].sort(),
  packages: rows.length,
  failing: failing.map((r) => `${r.name}@${r.version}: ${r.license ?? '<none>'}`),
  rows,
};
if (!checkOnly) {
  writeFileSync(join(repo, 'contracts/inventory/licenses.json'), `${JSON.stringify(out, null, 2)}\n`);
}
console.error(`${rows.length} third-party crates, ${failing.length} without a permissive license`);
for (const f of out.failing) console.error(`  ${f}`);
process.exit(failing.length === 0 ? 0 : 1);
