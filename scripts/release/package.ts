#!/usr/bin/env bun
// Build and package a Windows x64 release (WM-704).
//
//   bun scripts/release/package.ts [--skip-build]
//
// Produces dist/wrongmemory-<version>-windows-x64.zip and dist/SHA256SUMS.
// The binaries are NOT code-signed; release notes must say so.

import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';

const repo = resolve(import.meta.dir, '..', '..');
const skipBuild = process.argv.includes('--skip-build');

function run(cmd: string[], cwd = repo): void {
  const p = Bun.spawnSync(cmd, { cwd, stdout: 'inherit', stderr: 'inherit' });
  if (p.exitCode !== 0) throw new Error(`${cmd.join(' ')} exited ${p.exitCode}`);
}

const cargoToml = readFileSync(join(repo, 'Cargo.toml'), 'utf8');
const version = /\[workspace\.package\][^[]*?version\s*=\s*"([^"]+)"/s.exec(cargoToml)?.[1];
if (!version) throw new Error('workspace version not found');

if (!skipBuild) run(['cargo', 'build', '--release', '--locked', '-p', 'wrongmemoryd', '-p', 'wrongmemory-cli']);

const name = `wrongmemory-${version}-windows-x64`;
const dist = join(repo, 'dist');
const stage = join(dist, name);
rmSync(stage, { recursive: true, force: true });
mkdirSync(join(stage, 'bin'), { recursive: true });
for (const exe of ['wrongmemory.exe', 'wrongmemoryd.exe']) {
  const src = join(repo, 'target', 'release', exe);
  if (!existsSync(src)) throw new Error(`${src} missing; build first`);
  cpSync(src, join(stage, 'bin', exe));
}
for (const f of ['LICENSE', 'README.md']) cpSync(join(repo, f), join(stage, f));
mkdirSync(join(stage, 'docs'), { recursive: true });
for (const f of ['operations.md', 'runbooks.md', 'status.md']) cpSync(join(repo, 'docs', f), join(stage, 'docs', f));
cpSync(join(repo, 'contracts', 'profiles'), join(stage, 'contracts', 'profiles'), { recursive: true });

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((e) => {
    const p = join(dir, e);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}
const sha = (p: string) => createHash('sha256').update(readFileSync(p)).digest('hex');
const files = walk(stage).sort();
writeFileSync(
  join(stage, 'MANIFEST.json'),
  `${JSON.stringify(
    {
      name,
      version,
      signed: false,
      files: files.map((f) => ({ path: relative(stage, f).replaceAll('\\', '/'), sha256: sha(f), bytes: statSync(f).size })),
    },
    null,
    2,
  )}\n`,
);

const zip = join(dist, `${name}.zip`);
rmSync(zip, { force: true });
// Compress-Archive is available on every supported Windows host.
run([
  'powershell',
  '-NoProfile',
  '-Command',
  `Compress-Archive -Path '${stage}\\*' -DestinationPath '${zip}' -CompressionLevel Optimal`,
]);
// The two executables also ship as bare assets, so an installer (WrongStack's
// `wstack sage service install`) can verify and stage them without unzipping.
const bare: string[] = [];
for (const exe of ['wrongmemory', 'wrongmemoryd']) {
  const asset = `${exe}-windows-x64.exe`;
  cpSync(join(stage, 'bin', `${exe}.exe`), join(dist, asset));
  bare.push(`${sha(join(dist, asset))}  ${asset}\n`);
}
writeFileSync(join(dist, 'SHA256SUMS'), `${sha(zip)}  ${name}.zip\n${bare.join('')}`);
console.log(`${zip}\n${readFileSync(join(dist, 'SHA256SUMS'), 'utf8').trim()}`);
