# Windows x64 native desktop + CLI + daemon; per-user, no administrator needed.
#   irm https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install-desktop.ps1 | iex
# The same version, directory, PATH, autostart and attestation environment options
# as install.ps1 apply. No GUI is opened and no live store is adopted.
$PreviousDesktopOption = $env:WRONGMEMORY_DESKTOP
try {
  $env:WRONGMEMORY_DESKTOP = '1'
  $Ref = if ($env:WRONGMEMORY_VERSION) { 'v' + $env:WRONGMEMORY_VERSION.TrimStart('v') } else { 'main' }
  Invoke-Expression (Invoke-WebRequest -UseBasicParsing "https://raw.githubusercontent.com/WrongStack/wrongmemory/$Ref/install.ps1").Content
} finally {
  if ($null -eq $PreviousDesktopOption) { Remove-Item Env:WRONGMEMORY_DESKTOP -ErrorAction SilentlyContinue }
  else { $env:WRONGMEMORY_DESKTOP = $PreviousDesktopOption }
}
