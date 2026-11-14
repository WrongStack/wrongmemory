// Host platform description shared by desktop packaging, verification and tests.
// A package is always built and verified on the platform it targets.
export interface DesktopPlatform {
  id: string;
  triple: string;
  exe: string;
  archive: 'zip' | 'tar.gz';
}
const table: Record<string, DesktopPlatform> = {
  'win32-x64': { id: 'windows-x64', triple: 'x86_64-pc-windows-msvc', exe: '.exe', archive: 'zip' },
  'linux-x64': { id: 'linux-x64', triple: 'x86_64-unknown-linux-gnu', exe: '', archive: 'tar.gz' },
  'linux-arm64': { id: 'linux-arm64', triple: 'aarch64-unknown-linux-gnu', exe: '', archive: 'tar.gz' },
  'darwin-arm64': { id: 'macos-arm64', triple: 'aarch64-apple-darwin', exe: '', archive: 'tar.gz' },
  'darwin-x64': { id: 'macos-x64', triple: 'x86_64-apple-darwin', exe: '', archive: 'tar.gz' },
};
export function desktopPlatform(platform: string = process.platform, arch: string = process.arch): DesktopPlatform {
  const found = table[platform + '-' + arch];
  if (!found) throw new Error('Native desktop packaging does not support ' + platform + '-' + arch);
  return found;
}
export const binaryNames = (p: DesktopPlatform): string[] =>
  ['wrongmemory-desktop', 'wrongmemory', 'wrongmemoryd'].map(b => b + p.exe);
export const bareName = (p: DesktopPlatform): string => 'wrongmemory-desktop-' + p.id + p.exe;
