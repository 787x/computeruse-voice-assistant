# Run in an x64 MSVC Developer PowerShell with CMake, Ninja and LIBCLANG_PATH set.
param([string]$TargetDir = 'C:/cuava-cpu-target')
$ErrorActionPreference='Stop'
$env:CARGO_TARGET_DIR=$TargetDir
$env:CMAKE_GENERATOR='Ninja'
$env:CMAKE_TOOLCHAIN_FILE=(Join-Path $PSScriptRoot 'cpu.cmake')
# Portable Whisper baseline, independent of the build host CPU instruction set.
$env:GGML_NATIVE='OFF'
$env:GGML_AVX='OFF'
$env:GGML_AVX2='OFF'
$env:GGML_FMA='OFF'
$env:GGML_F16C='OFF'
# whisper-rs-sys does not track arbitrary GGML env changes; discard only its
# release build cache so a previous CUDA/native build cannot contaminate this package.
& cargo clean --manifest-path (Join-Path $PSScriptRoot '../Cargo.toml') --release -p whisper-rs-sys
if ($LASTEXITCODE -ne 0) { throw 'Whisper release cache cleanup failed' }
& cargo build --manifest-path (Join-Path $PSScriptRoot '../Cargo.toml') --release --locked --no-default-features
if ($LASTEXITCODE -ne 0) { throw 'CPU release build failed' }
Write-Host "CPU exe: $TargetDir/release/cuava.exe"
