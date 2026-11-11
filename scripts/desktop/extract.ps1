param([Parameter(Mandatory)][string]$Archive, [Parameter(Mandatory)][string]$Destination)
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $Destination) { throw 'Extraction target already exists' }
Expand-Archive -LiteralPath $Archive -DestinationPath $Destination
