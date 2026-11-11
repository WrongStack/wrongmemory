#!/usr/bin/env bun
import {createHash} from 'node:crypto';
import {existsSync, lstatSync, mkdtempSync, readFileSync} from 'node:fs';
import {isAbsolute, join, relative, resolve} from 'node:path';

const digest = (path: string) => createHash('sha256').update(readFileSync(path)).digest('hex');
function run(args: string[]): string {
  const result = Bun.spawnSync(args, {stdout:'pipe', stderr:'pipe', timeout:120000});
  if (result.exitCode !== 0) throw new Error(args[0] + ' failed: ' + result.stderr.toString());
  return result.stdout.toString();
}
export function verifyPackage(output: string, runtime = false): {extracted: string; files: number; version: string} {
  const root = resolve(output);
  const entries = readFileSync(join(root, 'SHA256SUMS'), 'utf8').trim().split(/\r?\n/).map(line => {
    const match = /^([a-f0-9]{64})  ([a-zA-Z0-9_.-]+)$/.exec(line);
    if (!match) throw new Error('Invalid checksum entry');
    if (digest(join(root, match[2]!)) !== match[1]) throw new Error('Checksum mismatch: ' + match[2]);
    return match[2]!;
  });
  const archives = entries.filter(file => file.endsWith('.zip'));
  if (archives.length !== 1) throw new Error('Expected one desktop ZIP');
  const scratch = mkdtempSync(join(root, 'verify-'));
  const extracted = join(scratch, 'extracted');
  run(['powershell','-NoProfile','-File',join(import.meta.dir,'extract.ps1'),'-Archive',join(root, archives[0]!),'-Destination',extracted]);
  const manifest = JSON.parse(readFileSync(join(extracted,'MANIFEST.json'),'utf8'));
  if (manifest.schema !== 'wrongmemory/desktop-package@1' || !Array.isArray(manifest.files)) throw new Error('Invalid manifest');
  const seen = new Set<string>();
  for (const file of manifest.files) {
    if (typeof file.path !== 'string' || seen.has(file.path)) throw new Error('Invalid/duplicate manifest path');
    seen.add(file.path);
    const path = resolve(extracted, file.path);
    const rel = relative(extracted,path);
    if (isAbsolute(file.path) || rel === '..' || rel.startsWith('../') || rel.startsWith('..\\') || isAbsolute(rel)) throw new Error('Manifest path escapes package');
    const stat = lstatSync(path);
    if (!stat.isFile() || stat.isSymbolicLink() || stat.size !== file.bytes || digest(path) !== file.sha256) throw new Error('Manifest mismatch: ' + file.path);
  }
  for (const name of ['wrongmemory-desktop.exe','wrongmemory.exe','wrongmemoryd.exe']) {
    const path = join(extracted,'bin',name);
    if (!seen.has('bin/' + name) || !existsSync(path)) throw new Error('Missing bundled binary: ' + name);
    if (runtime && !run([path,'--version']).trim().endsWith(' ' + manifest.version)) throw new Error('Extracted version mismatch: ' + name);
  }
  const bare = entries.find(file => file === 'wrongmemory-desktop-windows-x64.exe');
  if (!bare || digest(join(root,bare)) !== digest(join(extracted,'bin/wrongmemory-desktop.exe'))) throw new Error('Bare/extracted GUI mismatch');
  return {extracted, files:manifest.files.length, version:manifest.version};
}
if (import.meta.main) {
  const index = process.argv.indexOf('--out');
  if (index < 0 || !process.argv[index + 1]) throw new Error('Pass --out <package directory>');
  console.log(JSON.stringify(verifyPackage(process.argv[index + 1]!, process.argv.includes('--runtime')),null,2));
}
