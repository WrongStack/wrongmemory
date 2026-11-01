<#
.SYNOPSIS
  WM-701 upgrade smoke between built releases. Everything runs in a
  throwaway work directory: temp data roots, temp project roots, and a temp
  install prefix. Nothing touches %LOCALAPPDATA%, PATH, autostart, or a real
  store.

.DESCRIPTION
  Scenario A (install upgrade and rollback):
    1. Build the old release (-OldRef, default 40f7275, the first build with
       `install`) in a detached git worktree and install it into the prefix.
    2. With the installed old binaries: register a managed project, remember
       a fact, create a grant. The old daemon is left running.
    3. Run the current build's `install --prefix <p> --stop-daemon` over it.
    4. With the installed new binaries: the old fact is found, the grant is
       listed and still authenticates an MCP bridge, and a new fact is
       written.
    5. Roll back: stop the daemon, restore the *.exe.old binaries, and check
       that the old build still reads both facts.
  Scenario B (registry v1 data):
    Build -V1Ref (default b9c13b9, registry schema v1), write a project,
    a fact and a grant, stop its daemon, then open the same data root with
    the current build.

  Prints PASS/FAIL per check and exits non-zero on any failure.

.EXAMPLE
  pwsh scripts/upgrade/upgrade-smoke.ps1
#>
param(
  [string]$OldRef = '40f7275',
  [string]$V1Ref = 'b9c13b9',
  [string]$WorkDir = (Join-Path ([IO.Path]::GetTempPath()) ("wm-upgrade-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))),
  [switch]$Keep
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$failures = 0
$results = [System.Collections.Generic.List[string]]::new()

function Check([string]$name, [bool]$ok, [string]$detail = '') {
  $line = if ($ok) { "PASS  $name" } else { "FAIL  $name $detail" }
  $script:results.Add($line)
  Write-Host $line
  if (-not $ok) { $script:failures++ }
}

function Build-Ref([string]$ref) {
  $src = Join-Path $WorkDir "src-$ref"
  $target = Join-Path $WorkDir "target-$ref"
  git -C $repo worktree add --detach $src $ref | Out-Null
  $env:CARGO_TARGET_DIR = $target
  try {
    cargo build --locked -p wrongmemoryd -p wrongmemory-cli --manifest-path (Join-Path $src 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "build of $ref failed" }
  } finally {
    Remove-Item Env:CARGO_TARGET_DIR
  }
  return (Join-Path $target 'debug')
}

function Wm([string]$exe, [string]$data, [string[]]$wmArgs) {
  $out = & $exe --data-root $data --json @wmArgs 2>$null
  if ($LASTEXITCODE -ne 0) { throw "wrongmemory $($wmArgs -join ' ') failed with exit $LASTEXITCODE" }
  return ($out -join "`n") | ConvertFrom-Json
}

function Daemons([string]$data) {
  @(Get-CimInstance Win32_Process -Filter "Name='wrongmemoryd.exe'" |
      Where-Object { $_.CommandLine -and $_.CommandLine.Contains($data) })
}

function Wait-NoDaemon([string]$data) {
  $deadline = (Get-Date).AddSeconds(30)
  while ((Daemons $data).Count -gt 0) {
    if ((Get-Date) -gt $deadline) { throw "daemon for $data did not exit" }
    Start-Sleep -Milliseconds 200
  }
}

function Mcp-Search([string]$exe, [string]$data, [string]$project, [string]$grant, [string]$query) {
  $lines = @(
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"upgrade","version":"0"}}}',
    '{"jsonrpc":"2.0","method":"notifications/initialized"}',
    ('{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"' + $query + '"}}}')
  )
  $out = $lines | & $exe --data-root $data mcp --project $project --grant $grant --client upgrade 2>$null
  foreach ($l in $out) {
    $v = $l | ConvertFrom-Json
    if ($v.id -eq 2) { return $v }
  }
  return $null
}

New-Item -ItemType Directory -Force $WorkDir | Out-Null
Write-Host "work dir: $WorkDir"
try {
  # ── Builds ────────────────────────────────────────────────────────────────
  $oldBin = Build-Ref $OldRef
  $v1Bin = Build-Ref $V1Ref
  cargo build --locked -p wrongmemoryd -p wrongmemory-cli --manifest-path (Join-Path $repo 'Cargo.toml')
  if ($LASTEXITCODE -ne 0) { throw 'current build failed' }
  $newBin = Join-Path $repo 'target\debug'

  # ── Scenario A: install upgrade over a running old daemon, then rollback ──
  $dataA = Join-Path $WorkDir 'data-a'
  $projA = Join-Path $WorkDir 'proj-a'
  $prefix = Join-Path $WorkDir 'prefix'
  New-Item -ItemType Directory -Force $projA | Out-Null
  Wm (Join-Path $oldBin 'wrongmemory.exe') $dataA @('install', '--prefix', $prefix) | Out-Null
  $wm = Join-Path $prefix 'wrongmemory.exe'
  $oldHash = (Get-FileHash $wm).Hash
  $reg = Wm $wm $dataA @('project', 'add', $projA, '--managed')
  $pidA = $reg.project.projectId
  Wm $wm $dataA @('remember', $pidA, 'Upgrade keeps the release tag convention') | Out-Null
  $grantA = (Wm $wm $dataA @('grants', 'create', '--label', 'agent', '--project', $pidA)).grant.grantId
  Check 'A: old daemon running before the upgrade' ((Daemons $dataA).Count -eq 1)

  $inst = Wm (Join-Path $newBin 'wrongmemory.exe') $dataA @('install', '--prefix', $prefix, '--stop-daemon')
  Check 'A: install stopped the old daemon' ($inst.daemonStopped -eq $true) ($inst | ConvertTo-Json -Compress)
  Wait-NoDaemon $dataA
  Check 'A: installed binary is the new build' ((Get-FileHash $wm).Hash -eq (Get-FileHash (Join-Path $newBin 'wrongmemory.exe')).Hash)
  Check 'A: previous binaries kept as .exe.old' ((Test-Path "$wm.old") -and (Get-FileHash "$wm.old").Hash -eq $oldHash)

  $hits = Wm $wm $dataA @('search', $pidA, 'release tag convention')
  Check 'A: new build finds the old fact' (@($hits.hits).Count -eq 1) ($hits | ConvertTo-Json -Compress -Depth 6)
  $grants = Wm $wm $dataA @('grants', 'list')
  Check 'A: old grant still listed' (@($grants.grants | Where-Object { $_.grantId -eq $grantA }).Count -eq 1)
  $mcp = Mcp-Search $wm $dataA $pidA $grantA 'release tag'
  Check 'A: old grant authenticates an MCP bridge' ($null -ne $mcp -and $mcp.result.isError -eq $false) ($mcp | ConvertTo-Json -Compress -Depth 8)
  Wm $wm $dataA @('remember', $pidA, 'Written by the upgraded daemon') | Out-Null

  # Rollback to the old binaries over data the new build has written.
  Wm $wm $dataA @('daemon', 'stop') | Out-Null
  Wait-NoDaemon $dataA
  foreach ($b in 'wrongmemory.exe', 'wrongmemoryd.exe') {
    $p = Join-Path $prefix $b
    Move-Item -Force "$p.old" $p
  }
  Check 'A: rollback restored the old binary' ((Get-FileHash $wm).Hash -eq $oldHash)
  $back1 = Wm $wm $dataA @('search', $pidA, 'release tag convention')
  $back2 = Wm $wm $dataA @('search', $pidA, 'upgraded daemon')
  Check 'A: old build reads both facts after rollback' ((@($back1.hits).Count -eq 1) -and (@($back2.hits).Count -eq 1))
  Wm $wm $dataA @('daemon', 'stop') | Out-Null
  Wait-NoDaemon $dataA

  # ── Scenario B: registry v1 data opened by the current build ─────────────
  $dataB = Join-Path $WorkDir 'data-b'
  $projB = Join-Path $WorkDir 'proj-b'
  New-Item -ItemType Directory -Force $projB | Out-Null
  $v1 = Join-Path $v1Bin 'wrongmemory.exe'
  $pidB = (Wm $v1 $dataB @('project', 'add', $projB, '--managed')).project.projectId
  Wm $v1 $dataB @('remember', $pidB, 'Registry v1 era decision') | Out-Null
  $grantB = (Wm $v1 $dataB @('grants', 'create', '--label', 'agent', '--project', $pidB)).grant.grantId
  Wm $v1 $dataB @('daemon', 'stop') | Out-Null
  Wait-NoDaemon $dataB
  $newWm = Join-Path $newBin 'wrongmemory.exe'
  $hitsB = Wm $newWm $dataB @('search', $pidB, 'registry era decision')
  Check 'B: current build reads registry v1 data' (@($hitsB.hits).Count -eq 1) ($hitsB | ConvertTo-Json -Compress -Depth 6)
  $mcpB = Mcp-Search $newWm $dataB $pidB $grantB 'registry era'
  Check 'B: v1 grant authenticates an MCP bridge' ($null -ne $mcpB -and $mcpB.result.isError -eq $false) ($mcpB | ConvertTo-Json -Compress -Depth 8)
  Wm $newWm $dataB @('daemon', 'stop') | Out-Null
  Wait-NoDaemon $dataB
} catch {
  Check 'smoke completed' $false $_.Exception.Message
} finally {
  foreach ($d in (Get-ChildItem $WorkDir -Directory -Filter 'data-*' -ErrorAction SilentlyContinue)) {
    Daemons $d.FullName | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
  }
  foreach ($ref in $OldRef, $V1Ref) {
    git -C $repo worktree remove --force (Join-Path $WorkDir "src-$ref") 2>$null | Out-Null
  }
  if (-not $Keep) { Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("{0} checks, {1} failed" -f $results.Count, $failures)
exit ([int]($failures -gt 0))
