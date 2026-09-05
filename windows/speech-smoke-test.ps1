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
    $text = (& $binaryPath --data-dir $dataDir dictation transcribe $audio) -join ' '
    if ($LASTEXITCODE -ne 0) { throw "Transcription failed: $model" }
    if ($text -notmatch 'fellow Americans' -or $text -notmatch 'country') { throw "Unexpected $model transcript: $text" }
    Write-Host "${model}: local Windows transcription passed"
}
Remove-Item -LiteralPath $audio -Force
Write-Host 'Windows speech smoke test passed: verified model downloads, Whisper, Parakeet, WAV conversion, transcript output.'
