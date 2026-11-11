#!/usr/bin/env bun
// Optional native package: no install, update, publication or running-app replacement.
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { basename, dirname, isAbsolute, join, relative, resolve } from 'node:path';

const repo = resolve(import.meta.dir, '../..');
const native = join(repo, 'apps/desktop');
const sha = (path: string) => createHash('sha256').update(readFileSync(path)).digest('hex');
function run(args: string[], cwd = repo, capture = false): string {
  const result = Bun.spawnSync(args, { cwd, stdout: capture ? 'pipe' : 'inherit', stderr: 'inherit' });
  if (result.exitCode !== 0) throw new Error(args[0] + ' failed (' + result.exitCode + ')');
  return capture ? result.stdout.toString() : '';
}
export function manifestVersion(source: string, section: string): string {
  const start = source.indexOf('[' + section + ']');
  if (start < 0) throw new Error('Missing ' + section);
  const body = source.slice(start + section.length + 2).split(/\r?\n\[/, 1)[0]!;
  const version = /^\s*version\s*=\s*"(\d+\.\d+\.\d+)"/m.exec(body)?.[1];
  if (!version) throw new Error('Missing numeric version in ' + section);
  return version;
}
function inside(parent: string, path: string): boolean {
  const rel = relative(parent, path);
  return rel !== '..' && !rel.startsWith('..' + (process.platform === 'win32' ? '\\' : '/')) && !isAbsolute(rel);
}
function regular(path: string): void {
  const stat = lstatSync(path);
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error('Expected regular file: ' + path);
}
export interface PackageInput {
  output: string;
  version: string;
  binaries: Record<string, string>;
  license: string;
  readme: string;
  notices: string;
  licenseTexts?: string;
  profile: 'debug' | 'release';
  source?: { commit: string; dirty: boolean };
}
export function stagePackage(input: PackageInput): { stage: string; archive: string; bare: string } {
  if (process.platform !== 'win32' || process.arch !== 'x64') throw new Error('Native packaging currently supports Windows x64 only');
  const output = resolve(input.output);
  mkdirSync(output, { recursive: true });
  const name = 'wrongmemory-desktop-' + input.version + '-windows-x64';
  const archive = join(output, name + '.zip');
  const bare = join(output, 'wrongmemory-desktop-windows-x64.exe');
  if (existsSync(archive) || existsSync(bare) || existsSync(join(output, 'SHA256SUMS'))) throw new Error('Package output already exists; use a fresh --out directory');
  const stage = join(mkdtempSync(join(output, 'stage-')), name);
  if (!inside(output, stage)) throw new Error('Stage escaped its output directory');
  mkdirSync(join(stage, 'bin'), { recursive: true });
  const required = ['wrongmemory-desktop.exe', 'wrongmemory.exe', 'wrongmemoryd.exe'];
  for (const file of required) {
    const source = input.binaries[file];
    if (!source) throw new Error('Missing ' + file);
    regular(source); copyFileSync(source, join(stage, 'bin', file));
  }
  const files = ['LICENSE', 'README.md', 'THIRD_PARTY.json', ...required.map(f => 'bin/' + f)];
  for (const [source, file] of [[input.license, 'LICENSE'], [input.readme, 'README.md'], [input.notices, 'THIRD_PARTY.json']]) {
    regular(source!); copyFileSync(source!, join(stage, file!));
  }
  if (input.licenseTexts) {
    mkdirSync(join(stage, 'licenses'));
    for (const file of readdirSync(input.licenseTexts)) {
      const source = join(input.licenseTexts, file);
      regular(source); copyFileSync(source, join(stage, 'licenses', file)); files.push('licenses/' + file);
    }
  }
  writeFileSync(join(stage, 'MANIFEST.json'), JSON.stringify({
    schema: 'wrongmemory/desktop-package@1', name, version: input.version,
    platform: 'windows-x64', profile: input.profile, signed: false,
    desktopToolchain: '1.99.1', coreToolchain: '1.93.0', framework: 'gpui-kit@0.9.1',
    source: input.source ?? null,
    files: files.sort().map(file => ({ path: file, sha256: sha(join(stage, file)), bytes: lstatSync(join(stage, file)).size })),
  }, null, 2) + '\n');
  run(['powershell', '-NoProfile', '-File', join(repo, 'scripts/desktop/archive.ps1'), '-Source', stage, '-Destination', archive]);
  copyFileSync(input.binaries['wrongmemory-desktop.exe']!, bare);
  writeFileSync(join(output, 'SHA256SUMS'), sha(archive) + '  ' + basename(archive) + '\n' + sha(bare) + '  ' + basename(bare) + '\n');
  return { stage, archive, bare };
}

interface Crate { id: string; name: string; version: string; license: string | null; repository: string | null; manifest_path: string; license_file?: string; authors: string[]; source: string | null }
export function licenseInventory(output: string): string {
  const metadata = JSON.parse(run(['cargo', 'metadata', '--format-version', '1', '--locked', '--filter-platform', 'x86_64-pc-windows-msvc'], native, true));
  const active = new Set<string>(metadata.resolve.nodes.map((node: { id: string }) => node.id));
  const packages = (metadata.packages as Crate[]).filter(pkg => active.has(pkg.id));
  const root = join(output, 'licenses');
  mkdirSync(root, { recursive: true });
  const entries = packages.map(pkg => {
    if (!pkg.license) throw new Error('No declared license: ' + pkg.name + '@' + pkg.version);
    const crateRoot = dirname(pkg.manifest_path);
    const candidates = readdirSync(crateRoot).filter(file => /^(licen[cs]e|copying|notice)([.-]|$)/i.test(file)).map(file => join(crateRoot, file));
    if (pkg.license_file) {
      const path = resolve(crateRoot, pkg.license_file);
      if (!inside(crateRoot, path)) throw new Error('License path escapes crate: ' + pkg.name);
      candidates.push(path);
    }
    if (inside(repo, crateRoot) && pkg.name.startsWith('wrongmemory')) candidates.push(join(repo, 'LICENSE'));
    const texts: string[] = [];
    for (const path of new Set(candidates)) {
      const stat = lstatSync(path);
      if (!stat.isFile() || stat.isSymbolicLink()) continue;
      if (stat.size > 2 * 1024 * 1024) throw new Error('Oversized license text: ' + pkg.name);
      const name = pkg.name + '-' + pkg.version + '-' + basename(path);
      copyFileSync(path, join(root, name)); texts.push('licenses/' + name);
    }
    const vendorRoot=join(native,'third-party-licenses');
    const supplemental=JSON.parse(readFileSync(join(vendorRoot,'SOURCES.json'),'utf8')).packages.find((p: Crate)=>p.name===pkg.name&&p.version===pkg.version);
    for(const attachment of supplemental?.files??[]){
      if(basename(attachment.file)!==attachment.file)throw new Error('Invalid supplemental license path');
      const source=join(vendorRoot,attachment.file);regular(source);
      if(sha(source)!==attachment.sha256)throw new Error('Supplemental license checksum mismatch');
      copyFileSync(source,join(root,attachment.file));texts.push('licenses/'+attachment.file);
    }
    if(!texts.length)throw new Error('Missing license material for '+pkg.name+'@'+pkg.version);
    return { name: pkg.name, version: pkg.version, license: pkg.license, repository: pkg.repository, authors: pkg.authors,
      sourceArchive: pkg.source?.startsWith('registry+') ? 'https://crates.io/api/v1/crates/' + pkg.name + '/' + pkg.version + '/download' : null,
      licenseTextStatus: supplemental?.declarationOnly ? 'upstream-declaration-standard-text-and-source' : 'included', supplementalSources:supplemental?.files??[], texts };
  }).sort((a, b) => (a.name + '@' + a.version).localeCompare(b.name + '@' + b.version));
  const path = join(output, 'THIRD_PARTY.json');
  writeFileSync(path, JSON.stringify({ schema: 'wrongmemory/desktop-licenses@1', platform: 'windows-x64', packages: entries }, null, 2) + '\n');
  return path;
}
function main(): void {
  const args = process.argv.slice(2);
  const index = args.indexOf('--out');
  if (index >= 0 && !args[index + 1]) throw new Error('--out needs a directory');
  const output = resolve(index >= 0 ? args[index + 1]! : join(repo, 'dist-desktop'));
  if (existsSync(output) && readdirSync(output).length > 0) throw new Error('Package output already exists; use a fresh --out directory');
  const profile = args.includes('--debug') ? 'debug' : 'release';
  const version = manifestVersion(readFileSync(join(native, 'Cargo.toml'), 'utf8'), 'package');
  if (manifestVersion(readFileSync(join(repo, 'Cargo.toml'), 'utf8'), 'workspace.package') !== version) throw new Error('Desktop/core versions differ');
  if (!args.includes('--skip-build')) {
    const mode = profile === 'release' ? ['--release'] : [];
    run(['cargo', 'build', '--locked', ...mode, '-p', 'wrongmemory-cli', '-p', 'wrongmemoryd']);
    run(['cargo', 'build', '--locked', ...mode], native);
  }
  mkdirSync(output, { recursive: true });
  const notices = licenseInventory(output);
  const binaries = {
    'wrongmemory-desktop.exe': join(repo, 'target/desktop', profile, 'wrongmemory-desktop.exe'),
    'wrongmemory.exe': join(repo, 'target', profile, 'wrongmemory.exe'),
    'wrongmemoryd.exe': join(repo, 'target', profile, 'wrongmemoryd.exe'),
  };
  for (const path of Object.values(binaries)) {
    const banner = run([path, '--version'], repo, true).trim();
    if (!banner.endsWith(' ' + version)) throw new Error('Binary version mismatch: ' + basename(path));
  }
  const result = stagePackage({
    output, version, profile,
    binaries,
    license: join(repo, 'LICENSE'), readme: join(native, 'README.md'), notices, licenseTexts: join(output, 'licenses'),
    source: { commit: run(['git', 'rev-parse', 'HEAD'], repo, true).trim(), dirty: run(['git', 'status', '--porcelain'], repo, true).trim().length > 0 },
  });
  console.log(result.archive);
  console.log(readFileSync(join(output, 'SHA256SUMS'), 'utf8'));
}
if (import.meta.main) main();
