/**
 * Fails when the TS method list or ErrorCode set drifts from
 * `crates/wrongmemory-contracts/src/lib.rs`.
 */

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { ERROR_CODES, METHODS } from '../src/contracts.ts';

const src = readFileSync(
  join(import.meta.dir, '../../../crates/wrongmemory-contracts/src/lib.rs'),
  'utf8',
);

function section(open: RegExp): string {
  const m = open.exec(src);
  if (!m) throw new Error(`section ${open} not found`);
  let depth = 0;
  for (let i = m.index + m[0].length - 1; i < src.length; i++) {
    if (src[i] === '{') depth++;
    if (src[i] === '}' && --depth === 0) return src.slice(m.index, i + 1);
  }
  throw new Error(`section ${open} unterminated`);
}

const methodMod = section(/pub mod method\s*\{/);
const rustMethods = new Map<string, string>();
for (const m of methodMod.matchAll(/pub const ([A-Z_]+): &str = "([^"]+)";/g)) {
  rustMethods.set(m[1]!, m[2]!);
}

const errorEnum = section(/pub enum ErrorCode\s*\{/);
const rustCodes = [...errorEnum.matchAll(/^\s{4}([A-Z][A-Za-z]*),/gm)].map((m) =>
  m[1]!.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toUpperCase(),
);

const problems: string[] = [];
const tsMethods = new Map(Object.entries(METHODS));
for (const [k, v] of rustMethods) {
  if (tsMethods.get(k) !== v) problems.push(`method ${k}: rust=${v} ts=${tsMethods.get(k) ?? '<missing>'}`);
}
for (const [k, v] of tsMethods) {
  if (!rustMethods.has(k)) problems.push(`method ${k}=${v} exists only in TS`);
}
const tsCodes = new Set<string>(ERROR_CODES);
for (const c of rustCodes) if (!tsCodes.has(c)) problems.push(`ErrorCode ${c} missing in TS`);
for (const c of tsCodes) if (!rustCodes.includes(c)) problems.push(`ErrorCode ${c} exists only in TS`);
if (rustMethods.size === 0 || rustCodes.length === 0) problems.push('parser found nothing');

if (problems.length > 0) {
  console.error(`contract parity FAILED:\n  ${problems.join('\n  ')}`);
  process.exit(1);
}
console.log(`contract parity OK: ${rustMethods.size} methods, ${rustCodes.length} error codes`);
