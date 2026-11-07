# WrongMemory installer (Windows x64).
#
#   irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.ps1 | iex
#
# Environment:
#   WRONGMEMORY_VERSION         release to install, e.g. 0.1.9 (default: latest)
#   WRONGMEMORY_INSTALL_DIR     install directory (default: %LOCALAPPDATA%\WrongMemory\bin)
#   WRONGMEMORY_DOWNLOAD_BASE   mirror serving the release assets (overrides the version URL)
#   WRONGMEMORY_STOP_DAEMON=1   stop a running WrongMemory daemon so its binaries can be replaced
#   WRONGMEMORY_AUTOSTART=1     start the daemon at sign-in (a per-user Run value)
#   WRONGMEMORY_NO_MODIFY_PATH=1  leave the user PATH untouched
#   WRONGMEMORY_VERIFY_ATTESTATION=1
#                               also verify GitHub build provenance (needs a signed-in `gh`)
#
# Downloads wrongmemory.exe and wrongmemoryd.exe, verifies both against the
# release's SHA256SUMS, then runs `wrongmemory install`, which copies them into
# the install directory, writes install.json and keeps the previous binaries as
# *.exe.old. The install directory then goes on the user PATH (unless told
# not to): newer CLIs do that inside `wrongmemory install`, and this script
# does it as well for releases that do not, so either way it is added once.
# It does not start the daemon. Works in Windows PowerShell 5.1 and
# PowerShell 7.

# Run a native program and return its exit code and combined output as text. In Windows
# PowerShell 5.1, redirecting a native program's stderr under ErrorActionPreference 'Stop'
# turns its first stderr line into a terminating error, so this runs under 'Continue'.
function Invoke-WrongMemoryNative([string]$File, [string[]]$Arguments) {
  $ErrorActionPreference = 'Continue'
  $Output = @(& $File @Arguments 2>&1 | ForEach-Object { "$_" })
  [pscustomobject]@{ Code = $LASTEXITCODE; Output = $Output }
}

function Install-WrongMemory {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'
  # Windows PowerShell 5.1 may still default to TLS 1.0/1.1, which GitHub refuses.
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $Repo = 'WrongStack/wrongmemory'
  # PROCESSOR_ARCHITEW6432 is set when this shell is a 32-bit or emulated process.
  $Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  if ($Arch -ne 'AMD64' -and $Arch -ne 'ARM64') {
    throw "WrongMemory ships for Windows x64; this is $Arch"
  }
  if ($Arch -eq 'ARM64') {
    Write-Host 'Windows on ARM: installing the x64 build, which runs under emulation.'
  }
  $Base = if ($env:WRONGMEMORY_DOWNLOAD_BASE) {
    $env:WRONGMEMORY_DOWNLOAD_BASE.TrimEnd('/')
  } elseif ($env:WRONGMEMORY_VERSION) {
    "https://github.com/$Repo/releases/download/v$($env:WRONGMEMORY_VERSION.TrimStart('v'))"
  } else {
    "https://github.com/$Repo/releases/latest/download"
  }
  $Prefix = if ($env:WRONGMEMORY_INSTALL_DIR) {
    [System.IO.Path]::GetFullPath($env:WRONGMEMORY_INSTALL_DIR)
  } elseif ($env:LOCALAPPDATA) {
    Join-Path $env:LOCALAPPDATA 'WrongMemory\bin'
  } else {
    throw 'LOCALAPPDATA is not set; set WRONGMEMORY_INSTALL_DIR'
  }

  $Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('wrongmemory-install-' + [guid]::NewGuid())
  New-Item -ItemType Directory -Path $Tmp | Out-Null
  try {
    $Sums = Join-Path $Tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$Base/SHA256SUMS" -OutFile $Sums -UseBasicParsing
    $SumLines = Get-Content $Sums

    foreach ($Exe in 'wrongmemory', 'wrongmemoryd') {
      $Asset = "$Exe-windows-x64.exe"
      $Expected = @()
      foreach ($Line in $SumLines) {
        $Parts = $Line.Trim() -split '\s+', 2
        if ($Parts.Count -eq 2 -and $Parts[1].TrimStart('*') -eq $Asset) {
          $Expected += $Parts[0].ToLower()
        }
      }
      if ($Expected.Count -ne 1) { throw "SHA256SUMS must contain exactly one entry for $Asset" }

      Write-Host "Downloading $Asset..."
      $File = Join-Path $Tmp $Asset
      Invoke-WebRequest -Uri "$Base/$Asset" -OutFile $File -UseBasicParsing
      $Actual = (Get-FileHash -Algorithm SHA256 $File).Hash.ToLower()
      if ($Expected[0] -ne $Actual) { throw "checksum mismatch for $Asset" }
      if ($env:WRONGMEMORY_VERIFY_ATTESTATION -eq '1') {
        if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "'gh' is required for WRONGMEMORY_VERIFY_ATTESTATION" }
        $Verify = Invoke-WrongMemoryNative 'gh' @('attestation', 'verify', $File, '--repo', $Repo)
        if ($Verify.Code -ne 0) { throw "build provenance attestation did not verify for $Asset" }
      }
      # `wrongmemory install` copies wrongmemory.exe and wrongmemoryd.exe from its own directory.
      Move-Item -Force $File (Join-Path $Tmp "$Exe.exe")
    }
    Write-Host 'Checksums match SHA256SUMS.'
    if ($env:WRONGMEMORY_VERIFY_ATTESTATION -eq '1') { Write-Host 'Build provenance attestations verified.' }

    $InstallArgs = @('--json', 'install', '--prefix', $Prefix)
    if ($env:WRONGMEMORY_STOP_DAEMON -eq '1') { $InstallArgs += '--stop-daemon' }
    $Install = Invoke-WrongMemoryNative (Join-Path $Tmp 'wrongmemory.exe') $InstallArgs
    if ($Install.Code -eq 6) {
      throw 'A WrongMemory daemon is running. Rerun with $env:WRONGMEMORY_STOP_DAEMON = "1" to stop it and replace the binaries.'
    }
    if ($Install.Code -ne 0) {
      $Install.Output | ForEach-Object { Write-Host $_ }
      throw "wrongmemory install exited $($Install.Code)"
    }
  } finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
  }

  $Cli = Join-Path $Prefix 'wrongmemory.exe'
  $Run = Invoke-WrongMemoryNative $Cli @('--version')
  $Version = $Run.Output | Select-Object -First 1
  if ($Run.Code -ne 0 -or -not $Version) { throw "The installed binary could not run: $Cli" }
  Write-Host ''
  Write-Host "Installed $Version -> $Prefix"

  if ($env:WRONGMEMORY_AUTOSTART -eq '1') {
    $Auto = Invoke-WrongMemoryNative $Cli @('autostart', 'enable', '--prefix', $Prefix)
    if ($Auto.Code -ne 0) { throw "autostart enable failed; run: $Cli autostart enable" }
    Write-Host 'The daemon starts at sign-in (wrongmemory autostart status).'
  }

  if (-not $env:WRONGMEMORY_NO_MODIFY_PATH) {
    if (Add-WrongMemoryUserPath -Dir $Prefix) {
      $env:Path = ($env:Path.TrimEnd(';') + ';' + $Prefix)
      Write-Host "Added $Prefix to your user PATH (new terminals pick it up)."
    }
  } elseif (-not (@($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -eq $Prefix.TrimEnd('\') })) {
    Write-Host "$Prefix is not on your PATH; call the binaries by full path or add it."
  }

  Write-Host ''
  Write-Host 'Next:'
  Write-Host '  wrongmemory doctor                         # check the install and the daemon'
  Write-Host '  wrongmemory ui                             # dashboard in the browser'
  Write-Host "  wstack sage service use wrongmemory        # use it as WrongStack's SAGE memory"
  Write-Host 'Update by running this installer again; remove with: wrongmemory uninstall'
}

# Append $Dir to the user PATH unless an entry already names it; true if it changed.
# Reads and writes the registry value itself so %VAR% entries stay unexpanded and the
# value keeps its REG_EXPAND_SZ type ([Environment]::SetEnvironmentVariable would expand
# them on read and store REG_SZ).
function Add-WrongMemoryUserPath([string]$Dir) {
  # Tests point this at a scratch key instead of the real user environment.
  $SubKey = if ($env:WRONGMEMORY_ENV_KEY_OVERRIDE) { $env:WRONGMEMORY_ENV_KEY_OVERRIDE } else { 'Environment' }
  $Key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($SubKey)
  try {
    $Raw = [string]$Key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $Want = $Dir.TrimEnd('\')
    $Entries = @($Raw -split ';' | Where-Object { $_ })
    foreach ($Entry in $Entries) {
      if ([Environment]::ExpandEnvironmentVariables($Entry).TrimEnd('\') -eq $Want) { return $false }
    }
    $Key.SetValue('Path', ((@($Entries) + $Dir) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
  } finally {
    $Key.Close()
  }
  if (-not $env:WRONGMEMORY_ENV_KEY_OVERRIDE) {
    # Tell Explorer (and so every new terminal) that the environment changed.
    if (-not ('WrongMemoryInstall.Native' -as [type])) {
      Add-Type -Namespace WrongMemoryInstall -Name Native -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);
'@
    }
    $Result = [UIntPtr]::Zero
    [void][WrongMemoryInstall.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$Result)
  }
  return $true
}

Install-WrongMemory
