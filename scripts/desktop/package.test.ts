import { afterAll, expect, test } from 'bun:test';
import { createHash } from 'node:crypto';
import { lstatSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, relative, resolve } from 'node:path';
import { manifestVersion, stagePackage } from './package.ts';
import { verifyPackage } from './verify.ts';
const scratch = mkdtempSync(join(tmpdir(), 'wm-desktop-package-'));
afterAll(() => {
  const target = resolve(scratch);
  if (!/^wm-desktop-package-[^\\/]+$/.test(relative(resolve(tmpdir()), target)) || lstatSync(target).isSymbolicLink()) throw new Error('Unsafe fixture cleanup target');
  rmSync(target, { recursive: true, force: true });
});
test('manifest version is scoped to the requested Cargo section', () => {
  expect(manifestVersion('[workspace.dependencies]\na = {version="9.9.9"}\n[package]\nversion="0.1.12"\n', 'package')).toBe('0.1.12');
  expect(() => manifestVersion('[package]\nname="x"', 'package')).toThrow('version');
});
test.skipIf(process.platform !== 'win32')('package keeps bundled clients and never overwrites previous output', () => {
  const binaries: Record<string,string> = {};
  for (const name of ['wrongmemory-desktop.exe', 'wrongmemory.exe', 'wrongmemoryd.exe']) {
    binaries[name] = join(scratch, name); writeFileSync(binaries[name]!, 'fixture:' + name);
  }
  const license = join(scratch, 'LICENSE'); writeFileSync(license, 'Fixture license');
  const readme = join(scratch, 'README'); writeFileSync(readme, 'Fixture documentation');
  const notices = join(scratch, 'THIRD_PARTY.json'); writeFileSync(notices, '{}');
  const input = { output: join(scratch, 'output with spaces'), version:'0.1.12', binaries, license, readme, notices, profile:'debug' as const };
  const result = stagePackage(input);
  const manifest = JSON.parse(readFileSync(join(result.stage,'MANIFEST.json'),'utf8'));
  expect(manifest.signed).toBe(false);
  expect(manifest.files.filter((f: {path: string}) => f.path.startsWith('bin/'))).toHaveLength(3);
  const digest = createHash('sha256').update(readFileSync(result.archive)).digest('hex');
  expect(readFileSync(join(input.output,'SHA256SUMS'),'utf8')).toContain(digest);
  expect(verifyPackage(input.output).files).toBe(6);
  expect(() => stagePackage(input)).toThrow('already exists');
  expect(createHash('sha256').update(readFileSync(result.archive)).digest('hex')).toBe(digest);
  const refused = Bun.spawnSync(['bun', join(import.meta.dir, 'package.ts'), '--skip-build', '--out', input.output]);
  expect(refused.exitCode).not.toBe(0);
  expect(refused.stderr.toString()).toContain('already exists');
  expect(createHash('sha256').update(readFileSync(result.archive)).digest('hex')).toBe(digest);
  writeFileSync(result.bare, 'modified binary');
  expect(() => verifyPackage(input.output)).toThrow('Checksum mismatch');
});
