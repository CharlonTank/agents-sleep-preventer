param([Parameter(Mandatory=$true)][string]$Binary)
$ErrorActionPreference = 'Stop'
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$dataDir = Join-Path $PSScriptRoot '../target/speech-test-data'
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
$dataDir = (Resolve-Path $dataDir).Path
$audio = Join-Path $dataDir 'jfk.wav'
Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/ggml-org/whisper.cpp/f049fff95a089aa9969deb009cdd4892b3e74916/samples/jfk.wav' -OutFile $audio -UseBasicParsing
foreach ($model in @('tiny', 'large-v3-turbo-q5_0', 'parakeet-v3')) {
    & $binaryPath --data-dir $dataDir dictation setup --model $model
    if ($LASTEXITCODE -ne 0) { throw "Model setup failed: $model" }
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $text = (& $binaryPath --data-dir $dataDir dictation transcribe $audio) -join ' '
    if ($LASTEXITCODE -ne 0) { throw "Transcription failed: $model" }
    if ($text -notmatch 'fellow Americans' -or $text -notmatch 'country') { throw "Unexpected $model transcript: $text" }
    Write-Host "${model}: local Windows transcription passed in $([Math]::Round($timer.Elapsed.TotalSeconds, 1)) seconds"
}
# Exercise both portable fallbacks even on an AVX2-capable runner.
foreach ($fallback in @(@{ Model = 'tiny'; Engine = 'whisper-cli' }, @{ Model = 'parakeet-v3'; Engine = 'parakeet-cli' })) {
    $optimized = Join-Path (Split-Path $binaryPath) ("speech/" + $fallback.Engine + '-avx2.exe')
    $disabled = "$optimized.disabled"
    Move-Item -LiteralPath $optimized -Destination $disabled
    try {
        & $binaryPath --data-dir $dataDir dictation configure --model $fallback.Model
        if ($LASTEXITCODE -ne 0) { throw 'Could not select fallback test model' }
        $text = (& $binaryPath --data-dir $dataDir dictation transcribe $audio) -join ' '
        if ($LASTEXITCODE -ne 0 -or $text -notmatch 'fellow Americans') { throw 'Portable CPU fallback failed' }
        Write-Host "$($fallback.Engine): portable CPU fallback transcription passed"
    } finally {
        Move-Item -LiteralPath $disabled -Destination $optimized
    }
}
Remove-Item -LiteralPath $audio -Force
Write-Host 'Windows speech smoke test passed: verified model downloads, Whisper, Parakeet, WAV conversion, transcript output.'
