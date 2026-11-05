$ErrorActionPreference='Stop'
$root=Join-Path ([IO.Path]::GetTempPath()) ('cuava-import-check-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$source=Join-Path $root 'agent.json'
$data=Join-Path $root 'target'
New-Item -ItemType Directory -Path $data | Out-Null
[IO.File]::WriteAllText($source, '{"providerId":"deepseek","model":"legacy-model","contextWindow":1000000,"apiKey":"NEVER-COPY","providerKeys":{"mimo":"enc:NEVER-COPY"}}')
[IO.File]::WriteAllText((Join-Path $data 'settings.json'), '{"model":"my-custom-model"}')
$sourceHash=(Get-FileHash -LiteralPath $source).Hash
$script=Join-Path $PSScriptRoot 'import-legacy.ps1'
& powershell -NoProfile -File $script -Source $source -DataDir $data | Out-Null
if ((Get-Content (Join-Path $data 'settings.json') -Raw) -ne '{"model":"my-custom-model"}') { throw 'Preview wrote target' }
'IMPORT' | & powershell -NoProfile -File $script -Source $source -DataDir $data -Apply | Out-Null
$target=Join-Path $data 'settings.json'
$value=Get-Content -LiteralPath $target -Raw
$json=$value | ConvertFrom-Json
if ($json.model -ne 'my-custom-model' -or $json.contextWindow -ne 1000000 -or $value.Contains('NEVER-COPY')) { throw 'Unsafe import result' }
if (@(Get-ChildItem -LiteralPath $data -Filter '*.bak').Count -ne 1) { throw 'Missing backup' }
$hash=(Get-FileHash -LiteralPath $target).Hash
'IMPORT' | & powershell -NoProfile -File $script -Source $source -DataDir $data -Apply | Out-Null
if ((Get-FileHash -LiteralPath $target).Hash -ne $hash) { throw 'Repeated import changed target' }
if ((Get-FileHash -LiteralPath $source).Hash -ne $sourceHash) { throw 'Source modified' }
Write-Host 'PASS: preview, backup, customized values, secret exclusion, source immutability, repeat import'
foreach ($case in @(@(9,$false),@(10,$true),@(99,$true),@(100,$false),@(1.5,$false))) {
    $inputPath=Join-Path $root ('boundary-'+[guid]::NewGuid().ToString('N')+'.json')
    [IO.File]::WriteAllText($inputPath, ('{"contextCompressPercent":'+$case[0]+'}'))
    & powershell -NoProfile -File $script -Source $inputPath -DataDir (Join-Path $root 'empty') *> $null
    if (($LASTEXITCODE -eq 0) -ne $case[1]) { throw "Boundary failed: $($case[0])" }
}
Write-Host 'PASS: compression 9/10/99/100/fraction boundaries'
