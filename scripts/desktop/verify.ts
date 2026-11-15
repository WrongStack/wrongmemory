#!/usr/bin/env bun
import {createHash} from 'node:crypto';
import {existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync} from 'node:fs';
import {isAbsolute, join, relative, resolve} from 'node:path';
import {bareName, binaryNames, desktopPlatform} from './platform.ts';

const digest = (path: string) => createHash('sha256').update(readFileSync(path)).digest('hex');
function run(args: string[]): string {
  // Windows PowerShell 5.1 started from PowerShell 7 inherits a PSModulePath
  // that loads an incompatible Archive module; let 5.1 use its own default.
  const env = {...process.env};
  if (args[0] === 'powershell') delete env.PSModulePath;
  const result = Bun.spawnSync(args, {stdout:'pipe', stderr:'pipe', timeout:120000, env});
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
  const platform = desktopPlatform();
  const archives = entries.filter(file => file.endsWith('.' + platform.archive));
  if (archives.length !== 1) throw new Error('Expected one desktop ' + platform.archive + ' archive');
  const scratch = mkdtempSync(join(root, 'verify-'));
  const extracted = join(scratch, 'extracted');
  if (platform.archive === 'zip') {
    run(['powershell','-NoProfile','-File',join(import.meta.dir,'extract.ps1'),'-Archive',join(root, archives[0]!),'-Destination',extracted]);
  } else {
    mkdirSync(extracted);
    run(['tar','-xzf',join(root, archives[0]!),'-C',extracted]);
  }
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
  for (const name of binaryNames(platform)) {
    const path = join(extracted,'bin',name);
    if (!seen.has('bin/' + name) || !existsSync(path)) throw new Error('Missing bundled binary: ' + name);
    if (runtime && !run([path,'--version']).trim().endsWith(' ' + manifest.version)) throw new Error('Extracted version mismatch: ' + name);
  }
  const bare = entries.find(file => file === bareName(platform));
  if (!bare || digest(join(root,bare)) !== digest(join(extracted,'bin',binaryNames(platform)[0]!))) throw new Error('Bare/extracted GUI mismatch');
  return {extracted, files:manifest.files.length, version:manifest.version};
}
if (import.meta.main) {
  const index = process.argv.indexOf('--out');
  if (index < 0 || !process.argv[index + 1]) throw new Error('Pass --out <package directory>');
  console.log(JSON.stringify(verifyPackage(process.argv[index + 1]!, process.argv.includes('--runtime')),null,2));
}
