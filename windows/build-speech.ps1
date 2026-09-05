param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
# Exact whisper.cpp v1.9.1 source revision; both engines use static MSVC CRT.
$revision = 'f049fff95a089aa9969deb009cdd4892b3e74916'
$root = Join-Path $PSScriptRoot '../target/windows-speech-source'
$source = Join-Path $root 'source'
$build = Join-Path $root 'build'
if (-not (Test-Path -LiteralPath (Join-Path $source 'CMakeLists.txt'))) {
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    git clone --no-checkout --filter=blob:none https://github.com/ggml-org/whisper.cpp.git $source
    if ($LASTEXITCODE -ne 0) { throw 'Could not fetch speech engine source' }
    git -C $source checkout --detach $revision
    if ($LASTEXITCODE -ne 0) { throw 'Could not select speech engine revision' }
}
$actual = git -C $source rev-parse HEAD
if ($actual.Trim() -ne $revision) { throw 'Unexpected speech engine source revision' }
cmake -S $source -B $build -A x64 -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded -DBUILD_SHARED_LIBS=OFF -DGGML_STATIC=ON -DGGML_OPENMP=OFF -DGGML_NATIVE=OFF -DGGML_AVX=OFF -DGGML_AVX2=OFF -DGGML_FMA=OFF -DGGML_F16C=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_SDL2=OFF
if ($LASTEXITCODE -ne 0) { throw 'Could not configure speech engine build' }
cmake --build $build --config Release --target whisper-cli parakeet-cli --parallel 4
if ($LASTEXITCODE -ne 0) { throw 'Could not build speech engines' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
foreach ($engine in @('whisper-cli.exe', 'parakeet-cli.exe')) {
    Copy-Item -LiteralPath (Join-Path $build "bin/Release/$engine") -Destination $OutputDirectory -Force
}
Copy-Item -LiteralPath (Join-Path $source 'LICENSE') -Destination (Join-Path $OutputDirectory 'LICENSE-whisper.cpp.txt') -Force
Copy-Item -LiteralPath (Join-Path $source 'ggml/LICENSE') -Destination (Join-Path $OutputDirectory 'LICENSE-ggml.txt') -Force
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'SOURCE.txt'), "whisper.cpp v1.9.1`nhttps://github.com/ggml-org/whisper.cpp/tree/$revision`n")
