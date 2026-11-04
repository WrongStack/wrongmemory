#!/usr/bin/env bun
// Build and package a release for the host platform (WM-704, WM-802).
//
//   bun scripts/release/package.ts [--skip-build]
//
// Windows x64:  dist/wrongmemory-<version>-windows-x64.zip
//               dist/wrongmemory-windows-x64.exe, dist/wrongmemoryd-windows-x64.exe
// Linux x64:    dist/wrongmemory-<version>-linux-x64.tar.gz
//               dist/wrongmemory-linux-x64, dist/wrongmemoryd-linux-x64
// macOS arm64:  dist/wrongmemory-<version>-darwin-arm64.tar.gz
//               dist/wrongmemory-darwin-arm64, dist/wrongmemoryd-darwin-arm64
//
// plus dist/SHA256SUMS for this platform's files. The release workflow builds
// each platform on its own runner and writes one SHA256SUMS over all of them.
// The binaries are NOT code-signed (no Authenticode, no Apple notarization);
// release notes must say so.

import { createHash } from 'node:crypto';
import { chmodSync, cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';

const repo = resolve(import.meta.dir, '..', '..');
const skipBuild = process.argv.includes('--skip-build');

function run(cmd: string[], cwd = repo): void {
  const p = Bun.spawnSync(cmd, { cwd, stdout: 'inherit', stderr: 'inherit' });
  if (p.exitCode !== 0) throw new Error(`${cmd.join(' ')} exited ${p.exitCode}`);
}

// Asset platform names follow WrongStack's own release assets
// (`wstack-windows-x64.exe`, `wstack-linux-x64`, `wstack-darwin-arm64`).
const platforms: Record<string, string> = {
  'win32-x64': 'windows-x64',
  'linux-x64': 'linux-x64',
  'darwin-arm64': 'darwin-arm64',
};
const platform = platforms[`${process.platform}-${process.arch}`];
if (!platform) throw new Error(`no release package for ${process.platform}-${process.arch}`);
const windows = process.platform === 'win32';
const exeSuffix = windows ? '.exe' : '';

const cargoToml = readFileSync(join(repo, 'Cargo.toml'), 'utf8');
const version = /\[workspace\.package\][^[]*?version\s*=\s*"([^"]+)"/s.exec(cargoToml)?.[1];
if (!version) throw new Error('workspace version not found');

if (!skipBuild) run(['cargo', 'build', '--release', '--locked', '-p', 'wrongmemoryd', '-p', 'wrongmemory-cli']);

const name = `wrongmemory-${version}-${platform}`;
const dist = join(repo, 'dist');
const stage = join(dist, name);
rmSync(stage, { recursive: true, force: true });
mkdirSync(join(stage, 'bin'), { recursive: true });
for (const exe of ['wrongmemory', 'wrongmemoryd']) {
  const src = join(repo, 'target', 'release', `${exe}${exeSuffix}`);
  if (!existsSync(src)) throw new Error(`${src} missing; build first`);
  cpSync(src, join(stage, 'bin', `${exe}${exeSuffix}`));
  if (!windows) chmodSync(join(stage, 'bin', exe), 0o755);
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
// Archive modes do not depend on the checkout's (a checkout on a Windows
// mount reads as 0777): executables 0755, everything else 0644.
if (!windows) {
  const dirs = (dir: string): string[] =>
    readdirSync(dir).flatMap((e) => {
      const p = join(dir, e);
      return statSync(p).isDirectory() ? [p, ...dirs(p)] : [];
    });
  for (const d of [stage, ...dirs(stage)]) chmodSync(d, 0o755);
  for (const f of walk(stage)) chmodSync(f, relative(stage, f).startsWith('bin') ? 0o755 : 0o644);
}
const sha = (p: string) => createHash('sha256').update(readFileSync(p)).digest('hex');
const files = walk(stage).sort();
writeFileSync(
  join(stage, 'MANIFEST.json'),
  `${JSON.stringify(
    {
      name,
      version,
      platform,
      signed: false,
      files: files.map((f) => ({ path: relative(stage, f).replaceAll('\\', '/'), sha256: sha(f), bytes: statSync(f).size })),
    },
    null,
    2,
  )}\n`,
);

let archive: string;
if (windows) {
  archive = join(dist, `${name}.zip`);
  rmSync(archive, { force: true });
  // Compress-Archive is available on every supported Windows host.
  run([
    'powershell',
    '-NoProfile',
    '-Command',
    `Compress-Archive -Path '${stage}\\*' -DestinationPath '${archive}' -CompressionLevel Optimal`,
  ]);
} else {
  archive = join(dist, `${name}.tar.gz`);
  rmSync(archive, { force: true });
  // One top-level directory, as Unix archives usually have; modes are kept.
  run(['tar', '-czf', archive, '-C', dist, name]);
}
// The two executables also ship as bare assets, so an installer (WrongStack's
// `wstack sage service install`) can verify and stage them without unpacking.
const bare: string[] = [];
for (const exe of ['wrongmemory', 'wrongmemoryd']) {
  const asset = `${exe}-${platform}${exeSuffix}`;
  cpSync(join(stage, 'bin', `${exe}${exeSuffix}`), join(dist, asset));
  if (!windows) chmodSync(join(dist, asset), 0o755);
  bare.push(`${sha(join(dist, asset))}  ${asset}\n`);
}
const archiveName = archive.slice(dist.length + 1);
writeFileSync(join(dist, 'SHA256SUMS'), `${sha(archive)}  ${archiveName}\n${bare.join('')}`);
console.log(`${archive}\n${readFileSync(join(dist, 'SHA256SUMS'), 'utf8').trim()}`);
