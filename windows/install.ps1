# Run from an extracted AgentsSleepPreventer Windows ZIP. No admin rights needed.
$ErrorActionPreference = 'Stop'
$binary = Join-Path $PSScriptRoot 'asp.exe'
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw 'Extract the entire ZIP before installing.' }
& $binary install --yes
if ($LASTEXITCODE -ne 0) { throw "Installation failed (exit $LASTEXITCODE)" }
