param(
    [Parameter(Mandatory=$true)][string]$CpuExe,
    [Parameter(Mandatory=$true)][string]$MsvcRedist,
    [Parameter(Mandatory=$true)][string]$MakeNsis,
    [string]$Output = (Join-Path $PSScriptRoot '../../dist-rust')
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$Output = [IO.Path]::GetFullPath($Output)
New-Item -ItemType Directory -Force -Path $Output | Out-Null
# Unique stage: never recursively remove a computed directory.
$stage = Join-Path $Output "cuava-cpu-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item -LiteralPath $CpuExe -Destination (Join-Path $stage 'cuava.exe')
Copy-Item -LiteralPath (Join-Path $repo 'packages/cua/native/cua.ps1') -Destination $stage
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'import-legacy.ps1') -Destination $stage
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'README.md') -Destination $stage
foreach ($file in @('msvcp140.dll','vcruntime140.dll','vcruntime140_1.dll')) {
    Copy-Item -LiteralPath (Join-Path $MsvcRedist $file) -Destination $stage
}
# Copy every runtime DLL in the explicit licensed Microsoft CRT redist directory.
Get-ChildItem -LiteralPath $MsvcRedist -Filter '*.dll' | Copy-Item -Destination $stage
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
$inventory | Set-Content -Encoding UTF8 (Join-Path $licenses 'cargo-inventory.txt')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'REDISTRIBUTION.md') -Destination $licenses
$zip = Join-Path $Output 'cuava-0.1.0-windows-x64-cpu.zip'
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
$installer = Join-Path $Output 'cuava-0.1.0-windows-x64-cpu-setup.exe'
& $MakeNsis "/DSTAGE=$stage" "/DOUTPUT=$installer" (Join-Path $PSScriptRoot 'installer.nsi')
if ($LASTEXITCODE -ne 0) { throw 'NSIS failed' }
Get-FileHash -Algorithm SHA256 -LiteralPath $zip,$installer | Format-Table -AutoSize
Write-Host "未签名候选产物，尚需干净环境验收：$Output"
