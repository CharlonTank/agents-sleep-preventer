param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
# Exact whisper.cpp v1.9.1 source revision; both engines use static MSVC CRT.
$revision = 'f049fff95a089aa9969deb009cdd4892b3e74916'
$root = Join-Path $PSScriptRoot '../target/windows-speech-source'
$source = Join-Path $root 'source'
if (-not (Test-Path -LiteralPath (Join-Path $source 'CMakeLists.txt'))) {
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    git clone --no-checkout --filter=blob:none https://github.com/ggml-org/whisper.cpp.git $source
    if ($LASTEXITCODE -ne 0) { throw 'Could not fetch speech engine source' }
    git -C $source checkout --detach $revision
    if ($LASTEXITCODE -ne 0) { throw 'Could not select speech engine revision' }
}
$actual = git -C $source rev-parse HEAD
if ($actual.Trim() -ne $revision) { throw 'Unexpected speech engine source revision' }
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $visualStudio) { throw 'Could not find Visual Studio for dependency inspection' }
$dumpbin = Get-ChildItem (Join-Path $visualStudio 'VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe') | Sort-Object FullName -Descending | Select-Object -First 1
if (-not $dumpbin) { throw 'Could not find dumpbin for dependency inspection' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
foreach ($variant in @('baseline', 'avx2')) {
    $avx = if ($variant -eq 'avx2') { 'ON' } else { 'OFF' }
    $suffix = if ($variant -eq 'avx2') { '-avx2' } else { '' }
    $build = Join-Path $root ('build' + $suffix)
    # whisper.cpp's old minimum CMake version otherwise silently ignores the CRT
    # selection. Reset cached /MD flags from older builds as well.
    cmake --fresh -S $source -B $build -A x64 -DCMAKE_POLICY_DEFAULT_CMP0091=NEW -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded -DBUILD_SHARED_LIBS=OFF -DGGML_STATIC=ON -DGGML_OPENMP=OFF -DGGML_NATIVE=OFF "-DGGML_AVX=$avx" "-DGGML_AVX2=$avx" "-DGGML_FMA=$avx" "-DGGML_F16C=$avx" -DWHISPER_BUILD_TESTS=OFF -DWHISPER_SDL2=OFF
    if ($LASTEXITCODE -ne 0) { throw 'Could not configure speech engine build' }
    cmake --build $build --config Release --target whisper-cli parakeet-cli --parallel 4
    if ($LASTEXITCODE -ne 0) { throw 'Could not build speech engines' }
    foreach ($engine in @('whisper-cli', 'parakeet-cli')) {
        $executable = Join-Path $build "bin/Release/$engine.exe"
        $dependencies = (& $dumpbin.FullName /dependents $executable) -join "`n"
        if ($LASTEXITCODE -ne 0) { throw "Could not inspect $engine" }
        if ($dependencies -match '(?i)\b(?:MSVCP|VCRUNTIME|libomp)[\w-]*\.dll') {
            throw "The speech engine requires an external runtime: $engine`n$dependencies"
        }
        Write-Host "$engine dependency check passed: no external Visual C++ or OpenMP runtime."
        Copy-Item -LiteralPath $executable -Destination (Join-Path $OutputDirectory "$engine$suffix.exe") -Force
    }
}
Copy-Item -LiteralPath (Join-Path $source 'LICENSE') -Destination (Join-Path $OutputDirectory 'LICENSE-whisper.cpp.txt') -Force
# This revision includes ggml under the repository-wide MIT license above;
# it does not contain a separate ggml/LICENSE file.
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'SOURCE.txt'), "whisper.cpp v1.9.1`nhttps://github.com/ggml-org/whisper.cpp/tree/$revision`n")
