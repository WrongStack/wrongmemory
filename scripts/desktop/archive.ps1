param(
    [Parameter(Mandatory=$true)][string]$Source,
    [Parameter(Mandatory=$true)][string]$Destination
)
$ErrorActionPreference='Stop'
if(Test-Path -LiteralPath $Destination){throw 'Archive already exists; select a fresh output directory'}
# Cargo registry files can carry pre-1980 timestamps. Normalize the owned
# staging copy only; ZIP cannot represent those original timestamps.
Get-ChildItem -LiteralPath $Source -Recurse -Force | ForEach-Object {
    if ($_.LastWriteTimeUtc.Year -lt 1980) { $_.LastWriteTimeUtc = [datetime]'2000-01-01T00:00:00Z' }
}
$items=@(Get-ChildItem -LiteralPath $Source -Force | ForEach-Object {$_.FullName})
if($items.Count -eq 0){throw 'Cannot package an empty stage'}
Compress-Archive -LiteralPath $items -DestinationPath $Destination -CompressionLevel Optimal
