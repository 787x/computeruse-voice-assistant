param(
    [Parameter(Mandatory=$true)][string]$CudaRedist,
    [Parameter(Mandatory=$true)][string]$MsvcRedist,
    [Parameter(Mandatory=$true)][string]$MakeNsis,
    [string]$Output = (Join-Path $PSScriptRoot '../../dist-rust')
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $env:CUDA_PATH -or -not (Test-Path (Join-Path $env:CUDA_PATH 'bin/nvcc.exe'))) { throw '请设置 CUDA_PATH 指向构建使用的 CUDA Toolkit。' }
$cudaVersion = (Get-Content -Raw (Join-Path $env:CUDA_PATH 'version.json') | ConvertFrom-Json).cuda.version
if ($cudaVersion -ne '13.4.1') { throw "需要锁定的 CUDA Toolkit 13.4.1，实际为 $cudaVersion。" }
foreach ($tool in @('cargo','cmake','ninja','dumpbin','cl')) { if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "缺少 $tool；请使用 x64 MSVC Developer PowerShell。" } }
if (-not ((& rustc -vV) -match '^host: x86_64-pc-windows-msvc$')) { throw '需要 x86_64-pc-windows-msvc Rust 工具链。' }
$Output = [IO.Path]::GetFullPath($Output)
New-Item -ItemType Directory -Force -Path $Output | Out-Null
# Fresh target: the executable cannot come from a previous CPU build.
$build = Join-Path $Output "build-cuda-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $build | Out-Null
Get-ChildItem Env: | Where-Object { $_.Name -match '^(CMAKE_|GGML_|CUDACXX|CUDAHOSTCXX|CARGO_BUILD_TARGET|CARGO_ENCODED_RUSTFLAGS|RUSTFLAGS)' } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
$env:CARGO_TARGET_DIR = $build
$env:CMAKE_GENERATOR = 'Ninja'
$env:CUDACXX = Join-Path $env:CUDA_PATH 'bin/nvcc.exe'
# Keep the CPU fallback compatible with the prior portable baseline.
foreach ($flag in @('GGML_NATIVE','GGML_AVX','GGML_AVX2','GGML_FMA','GGML_F16C')) { Set-Item -Path "Env:$flag" -Value 'OFF' }
& cargo build --manifest-path (Join-Path $repo 'rust/Cargo.toml') --release --locked
if ($LASTEXITCODE -ne 0) { throw 'CUDA 构建失败；未生成发行包。' }
$exe = Join-Path $build 'release/cuava.exe'
$caches = @(Get-ChildItem (Join-Path $build 'release/build') -Filter CMakeCache.txt -Recurse | Where-Object { Select-String -Quiet -Path $_.FullName -Pattern '^GGML_CUDA:BOOL=ON$' })
if ($caches.Count -ne 1 -or -not (Test-Path $exe)) { throw '无法证明本次构建启用了 Whisper CUDA。' }
$compilerLine = Select-String -Path $caches[0].FullName -Pattern '^CMAKE_CUDA_COMPILER:FILEPATH=(.+)$'
if (-not $compilerLine -or ([IO.Path]::GetFullPath($compilerLine.Matches[0].Groups[1].Value) -ine [IO.Path]::GetFullPath($env:CUDACXX))) { throw '实际 CUDA 编译器与 CUDA_PATH 不一致。' }
$stage = Join-Path $Output "cuava-cuda-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'cuava.exe')
Copy-Item -LiteralPath (Join-Path $repo 'packages/cua/native/cua.ps1') -Destination $stage
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'import-legacy.ps1') -Destination $stage
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'README.md') -Destination $stage
foreach ($file in @('msvcp140.dll','vcruntime140.dll','vcruntime140_1.dll')) { Copy-Item -LiteralPath (Join-Path $MsvcRedist $file) -Destination $stage }
Get-ChildItem -LiteralPath $MsvcRedist -Filter '*.dll' | Copy-Item -Destination $stage
# CUDA files must come from a matching, licensed NVIDIA redistribution directory.
# Scan the executable and each collected NVIDIA DLL recursively, never System32.
$queue = [Collections.Generic.Queue[string]]::new()
$queue.Enqueue((Join-Path $stage 'cuava.exe'))
$seen = @{}
$nvidiaSources = @()
$imports = @()
while ($queue.Count -gt 0) {
    $binary = $queue.Dequeue()
    $lines = @(& dumpbin /dependents $binary)
    if ($LASTEXITCODE -ne 0) { throw "PE 依赖扫描失败: $binary" }
    $imports += "`n### $([IO.Path]::GetFileName($binary))`n" + ($lines -join "`n")
    foreach ($line in $lines) {
        if ($line -notmatch '^\s+([\w.-]+\.dll)\s*$') { continue }
        $name = $Matches[1]
        if ($name -notmatch '^(cudart|cublas|cublasLt|cufft|curand|cusolver|cusparse|nvJitLink|nvrtc|nvptxcompiler)') {
            if ($name -match '^(api-ms-win-|ext-ms-win-)' -or $name -ieq 'nvcuda.dll' -or (Test-Path -LiteralPath (Join-Path $stage $name)) -or (Test-Path -LiteralPath (Join-Path $env:SystemRoot "System32/$name"))) { continue }
            throw "无法解析 PE 导入 $name；请检查运行时依赖。"
        }
        if ($seen.ContainsKey($name)) { continue }
        $matches = @(Get-ChildItem -LiteralPath $CudaRedist -Filter $name -File -Recurse)
        if ($matches.Count -ne 1) { throw "NVIDIA redist 中必须有唯一的 $name，实际找到 $($matches.Count) 个。" }
        $toolkitDll = Join-Path $env:CUDA_PATH ("bin/x64/" + $name)
        if (-not (Test-Path -LiteralPath $toolkitDll) -or (Get-FileHash $toolkitDll).Hash -ne (Get-FileHash $matches[0].FullName).Hash) { throw "$name 与构建 Toolkit 不一致。" }
        $nvidiaSources += "NVIDIA source: $($matches[0].FullName) SHA256=$((Get-FileHash -LiteralPath $matches[0].FullName -Algorithm SHA256).Hash)"
        $seen[$name] = $true
        Copy-Item -LiteralPath $matches[0].FullName -Destination $stage
        $queue.Enqueue((Join-Path $stage $name))
    }
}
if (-not ($seen.Keys -match '^cublas')) { throw 'EXE 没有预期的 CUDA 导入，拒绝打包。' }
$licenseFiles = @(Get-ChildItem -LiteralPath $CudaRedist -File -Recurse | Where-Object { $_.Name -match '^(LICENSE|EULA)' })
if ($licenseFiles.Count -eq 0) { throw 'NVIDIA redist 缺少许可文件。' }
$imports | Set-Content -Encoding UTF8 (Join-Path $stage 'pe-imports.txt')
$licenses = Join-Path $stage 'licenses'
New-Item -ItemType Directory -Path $licenses | Out-Null
$metadata = (& cargo metadata --manifest-path (Join-Path $repo 'rust/Cargo.toml') --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
$inventory = @()
foreach ($package in $metadata.packages) {
    $inventory += "$($package.name) $($package.version) $($package.license) $($package.repository)"
    $dir = Split-Path $package.manifest_path
    $nested = Join-Path $dir 'LICENSES'
    if (Test-Path -LiteralPath $nested) {
        Copy-Item -LiteralPath $nested -Destination (Join-Path $licenses "$($package.name)-$($package.version)-LICENSES") -Recurse
    }
    foreach ($item in (Get-ChildItem -LiteralPath $dir -File | Where-Object Name -Match '^(LICENSE|COPYING|NOTICE)')) {
        Copy-Item -LiteralPath $item.FullName -Destination (Join-Path $licenses "$($package.name)-$($package.version)-$($item.Name)")
    }
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'native-notices') -Destination (Join-Path $licenses 'native') -Recurse
$nvidiaLicenses = Join-Path $licenses 'nvidia'
New-Item -ItemType Directory -Path $nvidiaLicenses | Out-Null
$i = 0
foreach ($license in $licenseFiles) { $i++; Copy-Item -LiteralPath $license.FullName -Destination (Join-Path $nvidiaLicenses ("{0}-{1}" -f $i,$license.Name)) }
$inventory | Set-Content -Encoding UTF8 (Join-Path $licenses 'cargo-inventory.txt')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'REDISTRIBUTION.md') -Destination $licenses
$nvcc = (& (Join-Path $env:CUDA_PATH 'bin/nvcc.exe') --version) -join "`n"
$record = @("Whisper: CUDA enabled; CPU runtime fallback", "source commit: $((& git -C $repo rev-parse HEAD))", "source status: $((& git -C $repo status --short) -join ' | ')", "CUDA Toolkit: $cudaVersion ($env:CUDA_PATH)", "NVIDIA redist: $CudaRedist", "MSVC redist: $MsvcRedist", $nvcc, "", "SHA-256:")
$record += $nvidiaSources
$record += Get-ChildItem -LiteralPath $stage -File -Recurse | Get-FileHash -Algorithm SHA256 | ForEach-Object { "$($_.Hash)  $($_.Path)" }
$record | Set-Content -Encoding UTF8 (Join-Path $stage 'build-manifest.txt')
$uninstall = Get-ChildItem -LiteralPath $stage -File | ForEach-Object { 'Delete "$INSTDIR\' + $_.Name + '"' }
$uninstall | Set-Content -Encoding ASCII (Join-Path $stage 'uninstall-files.nsh')
$zip = Join-Path $Output 'cuava-0.1.0-windows-x64-cuda.zip'
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
$installer = Join-Path $Output 'cuava-0.1.0-windows-x64-cuda-setup.exe'
& $MakeNsis "/DSTAGE=$stage" "/DOUTPUT=$installer" (Join-Path $PSScriptRoot 'installer.nsi')
if ($LASTEXITCODE -ne 0) { throw 'NSIS failed' }
Get-FileHash -Algorithm SHA256 -LiteralPath $zip,$installer | Format-Table -AutoSize
