# WrongMemory installer: Windows x64 native desktop + CLI + daemon; per-user, no administrator needed.
#
#   irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install-desktop.ps1 | iex
#
# This sets WRONGMEMORY_DESKTOP=1 and runs install.ps1, so the same version, directory, PATH,
# autostart and attestation environment options apply (see install.ps1). No GUI is opened and
# no live store is adopted. The desktop bundle is published from v0.2.1; an earlier
# WRONGMEMORY_VERSION is refused instead of silently installing the headless build.
$PreviousDesktopOption = $env:WRONGMEMORY_DESKTOP
try {
  $env:WRONGMEMORY_DESKTOP = '1'
  $Ref = 'main'
  if ($env:WRONGMEMORY_VERSION) {
    $Pinned = $env:WRONGMEMORY_VERSION.TrimStart('v')
    $Parsed = $null
    if (-not [version]::TryParse(($Pinned -replace '[-+].*$', ''), [ref]$Parsed)) {
      throw "WRONGMEMORY_VERSION '$($env:WRONGMEMORY_VERSION)' is not a version such as 0.2.1"
    }
    if ($Parsed -lt [version]'0.2.1') {
      throw "The Windows desktop bundle is published from WrongMemory 0.2.1; $Pinned has none. Use install.ps1 for a headless $Pinned install."
    }
    $Ref = "v$Pinned"
  }
  Invoke-Expression (Invoke-WebRequest -UseBasicParsing "https://raw.githubusercontent.com/WrongStack/wrongmemory/$Ref/install.ps1").Content
} finally {
  if ($null -eq $PreviousDesktopOption) { Remove-Item Env:WRONGMEMORY_DESKTOP -ErrorAction SilentlyContinue }
  else { $env:WRONGMEMORY_DESKTOP = $PreviousDesktopOption }
}
