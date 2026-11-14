#!/usr/bin/env bun
// Runtime smoke test for the native desktop: render every view in both
// themes against a disposable daemon and fail on an early exit or a panic.
// It needs a display (Xvfb on Linux CI, the session on macOS/Windows) and
// never touches the user's data root.
//
//   bun scripts/desktop/smoke.ts --app <wrongmemory-desktop> --cli <wrongmemory> [--seconds 6] [--screenshots <dir>]
//
// --screenshots captures the whole screen once per view before the app is
// closed (macOS `screencapture`, Linux `import -window root` under X,
// Windows a GDI screen copy). Use it only on a disposable session such as a
// CI runner: it records everything visible on the screen.
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const VIEWS = ['Library', 'Review', 'Relations', 'Clients', 'Health'] as const;
const THEMES = ['light', 'dark'] as const;

function arg(name: string, fallback?: string): string {
  const index = process.argv.indexOf(name);
  const value = index >= 0 ? process.argv[index + 1] : fallback;
  if (!value) throw new Error('Missing ' + name);
  return value;
}

function capture(file: string): string | null {
  const env = { ...process.env };
  let args: string[];
  if (process.platform === 'darwin') args = ['screencapture', '-x', file];
  else if (process.platform === 'win32') {
    delete env.PSModulePath;
    const ps = `Add-Type -AssemblyName System.Windows.Forms,System.Drawing; $b=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds; $i=New-Object System.Drawing.Bitmap $b.Width,$b.Height; $g=[System.Drawing.Graphics]::FromImage($i); $g.CopyFromScreen($b.Location,[System.Drawing.Point]::Empty,$b.Size); $i.Save('${file.replace(/'/g, "''")}')`;
    args = ['powershell', '-NoProfile', '-Command', ps];
  } else args = ['import', '-window', 'root', file];
  const result = Bun.spawnSync(args, { stdout: 'pipe', stderr: 'pipe', timeout: 30000, env });
  return result.exitCode === 0 ? null : (args[0] + ' failed: ' + result.stderr.toString().trim());
}

function alive(pid: number): boolean {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

function cli(binary: string, root: string, args: string[]): any {
  const result = Bun.spawnSync([binary, '--data-root', root, '--json', ...args], { stdout: 'pipe', stderr: 'pipe', timeout: 60000 });
  if (result.exitCode !== 0) throw new Error('fixture command failed: ' + args.join(' ') + '\n' + result.stderr.toString());
  const out = result.stdout.toString().trim();
  return out ? JSON.parse(out) : null;
}

export async function smoke(app: string, binary: string, seconds: number, screenshots?: string): Promise<{ runs: number; screenshots: number }> {
  const scratch = mkdtempSync(join(tmpdir(), 'wm-desktop-smoke-'));
  const root = join(scratch, 'root');
  const project = join(scratch, 'project');
  mkdirSync(join(project, 'src'), { recursive: true });
  writeFileSync(join(project, 'src', 'main.rs'), 'fn main() {}\n');
  let runs = 0;
  let shots = 0;
  if (screenshots) mkdirSync(screenshots, { recursive: true });
  try {
    const projectId = cli(binary, root, ['project', 'add', project, '--managed']).project.projectId as string;
    cli(binary, root, ['remember', projectId, 'Release builds must run cargo fmt before packaging.', '--kind', 'convention', '--file', 'src/main.rs']);
    cli(binary, root, ['remember', projectId, 'Ünicode — long text that wraps across several lines in the inspector without clipping at any width.', '--kind', 'decision']);
    cli(binary, root, ['candidates', 'propose', projectId, 'Prefer exact dependency pins for every new crate.', '--kind', 'convention']);
    for (const theme of THEMES) {
      for (const view of VIEWS) {
        writeFileSync(join(root, 'desktop-ui.json'), JSON.stringify({
          version: 1, appearance: theme, project: projectId, view, status: 'active',
          graphZoom: 1, sidebarWidth: 240, libraryWidth: 480,
        }) + '\n');
        const log = join(scratch, `app-${theme}-${view}.log`);
        const child = Bun.spawn([app, '--data-root', root, '--cli', binary], {
          stdout: Bun.file(log), stderr: Bun.file(log + '.err'), env: { ...process.env, RUST_BACKTRACE: '1' },
        });
        const deadline = Date.now() + seconds * 1000;
        const shotAt = deadline - 1500;
        let shot = !screenshots;
        while (Date.now() < deadline && child.exitCode === null) {
          if (!shot && Date.now() >= shotAt) {
            shot = true;
            const problem = capture(join(screenshots!, `${theme}-${view}.png`));
            if (problem) console.warn(`warning: screenshot ${theme}/${view}: ${problem}`);
            else shots++;
          }
          await Bun.sleep(200);
        }
        const exited = child.exitCode;
        if (exited === null) child.kill();
        await child.exited;
        const output = readFileSync(log, 'utf8') + readFileSync(log + '.err', 'utf8');
        if (exited !== null || output.includes('panicked')) {
          throw new Error(`desktop ${exited !== null ? 'exited with ' + exited : 'panicked'} in ${theme}/${view}:\n${output.slice(-4000)}`);
        }
        runs++;
        console.log(`ok ${theme}/${view}`);
      }
    }
  } finally {
    // `daemon stop` only requests the stop and the discovery file disappears
    // before the process exits, so wait for the PID itself; Windows refuses
    // to delete files a live process holds.
    const status = Bun.spawnSync([binary, '--data-root', root, '--json', 'daemon', 'status'], { stdout: 'pipe', stderr: 'ignore', timeout: 10000 });
    let pid = 0;
    try { pid = Number(JSON.parse(status.stdout.toString()).pid) || 0; } catch { pid = 0; }
    Bun.spawnSync([binary, '--data-root', root, 'daemon', 'stop'], { stdout: 'ignore', stderr: 'ignore', timeout: 30000 });
    const stopDeadline = Date.now() + 30000;
    while (pid && alive(pid) && Date.now() < stopDeadline) await Bun.sleep(250);
    try {
      rmSync(scratch, { recursive: true, force: true, maxRetries: 20, retryDelay: 500 });
    } catch (e) {
      // The renders already passed; a leftover temp directory is not a desktop failure.
      console.warn('warning: could not remove ' + scratch + ': ' + (e as Error).message);
    }
  }
  return { runs, screenshots: shots };
}

if (import.meta.main) {
  const shotsDir = process.argv.includes('--screenshots') ? resolve(arg('--screenshots')) : undefined;
  const result = await smoke(resolve(arg('--app')), resolve(arg('--cli')), Number(arg('--seconds', '6')), shotsDir);
  console.log(JSON.stringify(result));
}
