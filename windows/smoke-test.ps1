param([string]$Binary = "$PSScriptRoot/../target/x86_64-pc-windows-msvc/release/asp.exe")
$ErrorActionPreference = 'Stop'
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$stateDir = Join-Path ([IO.Path]::GetTempPath()) ('asp-smoke-' + [Guid]::NewGuid())
$monitor = $null
$agents = @()
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

function Invoke-Asp {
    & $binaryPath --data-dir $stateDir @args
    if ($LASTEXITCODE -ne 0) { throw "asp failed: $args (exit $LASTEXITCODE)" }
}

function Assert-State([int]$working, [bool]$awake) {
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $state = Invoke-Asp status | ConvertFrom-Json
        if ($state.monitor_running -and $state.working_agents -eq $working -and $state.sleep_prevention_requested -eq $awake) {
            if (-not $elevated) { return }
            # Verify the real OS request, beyond ASP's persisted marker state.
            $requests = (& powercfg.exe /requests 2>&1) -join "`n"
            if ($LASTEXITCODE -ne 0) { throw 'powercfg /requests failed' }
            # powercfg prints a device path (\Device\HarddiskVolume...) instead
            # of the drive letter. Match the unique repository path suffix.
            $suffix = $binaryPath.Substring([IO.Path]::GetPathRoot($binaryPath).Length)
            $held = $requests -match [regex]::Escape($suffix)
            if ($held -eq $awake) { return }
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Unexpected state: $($state | ConvertTo-Json -Compress); expected $working working, awake=$awake"
}

try {
    New-Item -ItemType Directory -Path $stateDir | Out-Null
    $monitor = Start-Process -FilePath $binaryPath -ArgumentList @('--data-dir', ('"' + $stateDir + '"'), 'daemon') -PassThru -WindowStyle Hidden
    Assert-State 0 $false
    # A second monitor must exit without taking ownership of the first one.
    $duplicate = Start-Process -FilePath $binaryPath -ArgumentList @('--data-dir', ('"' + $stateDir + '"'), 'daemon') -PassThru -WindowStyle Hidden
    if (-not $duplicate.WaitForExit(5000) -or $duplicate.ExitCode -ne 0) { throw 'Duplicate monitor did not exit successfully' }
    for ($i = 0; $i -lt 2; $i++) {
        $agents += Start-Process powershell.exe -ArgumentList '-NoProfile -Command Start-Sleep -Seconds 90' -PassThru -WindowStyle Hidden
    }
    Invoke-Asp start --pid $agents[0].Id
    Invoke-Asp start --pid $agents[1].Id
    Assert-State 2 $true
    Invoke-Asp stop --pid $agents[0].Id
    Assert-State 1 $true
    Invoke-Asp attention --pid $agents[1].Id
    Assert-State 0 $false
    Invoke-Asp refresh --pid $agents[1].Id
    Assert-State 1 $true
    Stop-Process -Id $agents[1].Id -Force
    Assert-State 0 $false
    Invoke-Asp force awake
    Assert-State 0 $true
    Invoke-Asp force sleep
    Assert-State 0 $false
    Invoke-Asp force auto
    Invoke-Asp quit
    if (-not $monitor.WaitForExit(5000)) { throw 'Monitor did not exit' }
    $finalState = Invoke-Asp status | ConvertFrom-Json
    if ($finalState.monitor_running -or $finalState.sleep_prevention_requested) { throw 'Sleep prevention remained active after quit' }
    Write-Host 'Windows smoke test passed: multi-agent activity, attention, process exit, overrides, singleton, shutdown.'
    if (-not $elevated) { Write-Host 'OS power request inspection skipped: run elevated to check powercfg /requests.' }
} finally {
    if ($monitor -and -not $monitor.HasExited) { Stop-Process -Id $monitor.Id -Force }
    foreach ($agent in $agents) { if (-not $agent.HasExited) { Stop-Process -Id $agent.Id -Force -ErrorAction SilentlyContinue } }
    Remove-Item -LiteralPath $stateDir -Recurse -Force -ErrorAction SilentlyContinue
}
