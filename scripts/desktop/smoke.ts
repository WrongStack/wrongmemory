#!/usr/bin/env bun
// Runtime smoke test for the native desktop: render every view in both
// themes against a disposable daemon and fail on an early exit, a crash or a
// missing window. It needs a display (Xvfb on Linux CI, the session on
// macOS/Windows) and never touches the user's data root.
//
//   bun scripts/desktop/smoke.ts --app <wrongmemory-desktop> --cli <wrongmemory> [--seconds 6] [--screenshots <dir>] [--require-window]
//
// Each run must also show a visible top-level window of at least 600x400
// (Windows user32, macOS CoreGraphics, X11 xwininfo). If the platform probe
// cannot run, the check is skipped with a warning unless --require-window
// makes that a failure (CI does).
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

// A visible top-level window of the app, its size, or why none was found.
// `unavailable` means the platform probe itself could not run.
type WindowProbe = { width: number; height: number } | { missing: string; unavailable?: boolean };

const MIN_VISIBLE = { width: 600, height: 400 };
// Extra seconds for the first launch of a run (cold start).
const COLD_START_EXTRA_S = 15;

// Output that means the app crashed: a Rust panic, an uncaught Objective-C
// exception (macOS), or an abort from the Rust runtime.
const CRASH_MARKERS = ['panicked', 'uncaught exception', 'fatal runtime error'];

export function probeWindow(pid: number): WindowProbe {
  const env = { ...process.env };
  let args: string[];
  if (process.platform === 'win32') {
    delete env.PSModulePath;
    // MainWindowHandle is the first visible, unowned top-level window. The
    // class icon (GCLP_HICON) comes from the exe's embedded icon resource.
    const ps = `$p = Get-Process -Id ${pid} -ErrorAction SilentlyContinue; if (-not $p -or $p.MainWindowHandle -eq 0) { 'none'; exit }
Add-Type -Namespace WmSmoke -Name User32 -MemberDefinition '[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r); [DllImport("user32.dll")] public static extern bool IsIconic(System.IntPtr h); [DllImport("user32.dll")] public static extern System.IntPtr GetClassLongPtrW(System.IntPtr h, int i); public struct RECT { public int L, T, R, B; }'
$h = $p.MainWindowHandle; $r = New-Object WmSmoke.User32+RECT; [void][WmSmoke.User32]::GetWindowRect($h, [ref]$r)
if ([WmSmoke.User32]::IsIconic($h)) { 'minimized' } elseif ([WmSmoke.User32]::GetClassLongPtrW($h, -14) -eq 0) { 'without-icon' } else { '{0}x{1}' -f ($r.R - $r.L), ($r.B - $r.T) }`;
    args = ['powershell', '-NoProfile', '-Command', ps];
  } else if (process.platform === 'darwin') {
    // CoreGraphics reports owner PID and bounds without the screen-recording
    // permission. 1 = kCGWindowListOptionOnScreenOnly, 0 = kCGNullWindowID.
    const js = `ObjC.import('CoreGraphics');
const all = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(1, 0))) || [];
const mine = all.filter(w => w.kCGWindowOwnerPID === ${pid} && w.kCGWindowLayer === 0).map(w => w.kCGWindowBounds);
mine.length ? mine.map(b => Math.round(b.Width) + 'x' + Math.round(b.Height)).join(' ') : 'none';`;
    args = ['osascript', '-l', 'JavaScript', '-e', js];
  } else {
    // X11: the app sets WM_CLASS to its app id; xwininfo also lists unmapped
    // windows, so check each match's map state.
    const tree = Bun.spawnSync(['xwininfo', '-root', '-tree'], { stdout: 'pipe', stderr: 'pipe', timeout: 10000 });
    if (tree.exitCode !== 0) return { missing: 'xwininfo failed: ' + tree.stderr.toString().trim(), unavailable: true };
    const ids = tree.stdout.toString().split('\n')
      .filter(line => line.includes('"com.wrongstack.WrongMemory"'))
      .map(line => line.trim().split(/\s+/)[0]);
    for (const id of ids) {
      const info = Bun.spawnSync(['xwininfo', '-id', id], { stdout: 'pipe', stderr: 'ignore', timeout: 10000 }).stdout.toString();
      const width = Number(/Width:\s+(\d+)/.exec(info)?.[1]);
      const height = Number(/Height:\s+(\d+)/.exec(info)?.[1]);
      if (info.includes('Map State: IsViewable') && width >= MIN_VISIBLE.width && height >= MIN_VISIBLE.height) return { width, height };
    }
    return { missing: ids.length ? 'window not viewable or too small' : 'no window' };
  }
  const result = Bun.spawnSync(args, { stdout: 'pipe', stderr: 'pipe', timeout: 30000, env });
  if (result.exitCode !== 0) return { missing: args[0] + ' failed: ' + result.stderr.toString().trim(), unavailable: true };
  const out = result.stdout.toString().trim();
  for (const size of out.split(/\s+/)) {
    const match = /^(\d+)x(\d+)$/.exec(size);
    if (match && Number(match[1]) >= MIN_VISIBLE.width && Number(match[2]) >= MIN_VISIBLE.height) {
      return { width: Number(match[1]), height: Number(match[2]) };
    }
  }
  return { missing: out === 'none' ? 'no window' : 'window ' + out };
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

export async function smoke(app: string, binary: string, seconds: number, screenshots?: string, requireWindow = false): Promise<{ runs: number; screenshots: number; windows: number }> {
  const scratch = mkdtempSync(join(tmpdir(), 'wm-desktop-smoke-'));
  const root = join(scratch, 'root');
  const project = join(scratch, 'project');
  mkdirSync(join(project, 'src'), { recursive: true });
  writeFileSync(join(project, 'src', 'main.rs'), 'fn main() {}\n');
  let runs = 0;
  let shots = 0;
  let windows = 0;
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
        // The first launch of a freshly built binary is a cold start: on a
        // macOS runner it once showed no window within 6 s (ci run
        // 43710230091, passed on rerun), so it gets extra time.
        const deadline = Date.now() + (runs === 0 ? seconds + COLD_START_EXTRA_S : seconds) * 1000;
        const shotAt = deadline - 1500;
        let shot = !screenshots;
        const probeFrom = Date.now() + 1000;
        let seen: WindowProbe = { missing: 'not probed' };
        while (Date.now() < deadline && child.exitCode === null) {
          if ('missing' in seen && !seen.unavailable && Date.now() >= probeFrom) seen = probeWindow(child.pid);
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
        // A crashed macOS app can stay alive in the crash reporter past the
        // deadline, so its output is checked as well as its exit.
        const crash = CRASH_MARKERS.find(marker => output.includes(marker));
        if (exited !== null || crash) {
          const cause = output.split('\n').filter(line => CRASH_MARKERS.some(m => line.includes(m)) || line.includes('reason:')).slice(0, 5).join('\n');
          throw new Error(`desktop ${exited !== null ? 'exited with ' + exited : 'crashed (' + crash + ')'} in ${theme}/${view}:\n${cause}\n...\n${output.slice(-4000)}`);
        }
        let shown = '';
        if ('missing' in seen) {
          if (!seen.unavailable || requireWindow) {
            throw new Error(`desktop window check failed in ${theme}/${view}: ${seen.missing}\n${output.slice(-4000)}`);
          }
          console.warn(`warning: window check skipped: ${seen.missing}`);
        } else {
          windows++;
          shown = ` (${seen.width}x${seen.height})`;
        }
        runs++;
        console.log(`ok ${theme}/${view}${shown}`);
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
  return { runs, screenshots: shots, windows };
}

if (import.meta.main) {
  const shotsDir = process.argv.includes('--screenshots') ? resolve(arg('--screenshots')) : undefined;
  const result = await smoke(resolve(arg('--app')), resolve(arg('--cli')), Number(arg('--seconds', '6')), shotsDir, process.argv.includes('--require-window'));
  console.log(JSON.stringify(result));
}
