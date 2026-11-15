#!/usr/bin/env bun
// Regenerate the platform icon files from the one SVG source:
//   crates/wrongmemory-cli/assets/wrongmemory-desktop.svg  (Linux, source)
//   apps/desktop/assets/wrongmemory-desktop.ico            (Windows exe resource)
//   crates/wrongmemory-cli/assets/wrongmemory-desktop.icns (macOS WrongMemory.app)
//
//   bun scripts/desktop/build-icons.ts
//
// Needs `rsvg-convert` (librsvg) on PATH. Both containers store PNG images,
// which Windows Vista+ and macOS 10.7+ read directly.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const repo = resolve(import.meta.dir, '..', '..');
const svg = join(repo, 'crates/wrongmemory-cli/assets/wrongmemory-desktop.svg');
const ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 256];
// icns element types holding PNG data, by pixel size.
const ICNS_TYPES: [string, number][] = [
  ['ic11', 32], ['ic12', 64], ['ic07', 128], ['ic13', 256],
  ['ic08', 256], ['ic14', 512], ['ic09', 512], ['ic10', 1024],
];

function render(dir: string, size: number): Buffer {
  const file = join(dir, `${size}.png`);
  const result = Bun.spawnSync(['rsvg-convert', '-w', String(size), '-h', String(size), svg, '-o', file], { stderr: 'pipe' });
  if (result.exitCode !== 0) throw new Error('rsvg-convert failed: ' + result.stderr.toString());
  return readFileSync(file);
}

export function ico(images: [number, Buffer][]): Buffer {
  const header = Buffer.alloc(6 + 16 * images.length);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  let offset = header.length;
  images.forEach(([size, png], i) => {
    const entry = 6 + 16 * i;
    header.writeUInt8(size >= 256 ? 0 : size, entry);
    header.writeUInt8(size >= 256 ? 0 : size, entry + 1);
    header.writeUInt16LE(1, entry + 4); // planes
    header.writeUInt16LE(32, entry + 6); // bits per pixel
    header.writeUInt32LE(png.length, entry + 8);
    header.writeUInt32LE(offset, entry + 12);
    offset += png.length;
  });
  return Buffer.concat([header, ...images.map(([, png]) => png)]);
}

export function icns(elements: [string, Buffer][]): Buffer {
  const parts = elements.map(([type, png]) => {
    const head = Buffer.alloc(8);
    head.write(type, 0, 'ascii');
    head.writeUInt32BE(png.length + 8, 4);
    return Buffer.concat([head, png]);
  });
  const head = Buffer.alloc(8);
  head.write('icns', 0, 'ascii');
  head.writeUInt32BE(8 + parts.reduce((n, p) => n + p.length, 0), 4);
  return Buffer.concat([head, ...parts]);
}

if (import.meta.main) {
  const dir = mkdtempSync(join(tmpdir(), 'wm-icons-'));
  try {
    const cache = new Map<number, Buffer>();
    const png = (size: number) => cache.get(size) ?? cache.set(size, render(dir, size)).get(size)!;
    writeFileSync(join(repo, 'apps/desktop/assets/wrongmemory-desktop.ico'), ico(ICO_SIZES.map(s => [s, png(s)])));
    writeFileSync(join(repo, 'crates/wrongmemory-cli/assets/wrongmemory-desktop.icns'), icns(ICNS_TYPES.map(([t, s]) => [t, png(s)])));
    console.log('wrote wrongmemory-desktop.ico and wrongmemory-desktop.icns');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
