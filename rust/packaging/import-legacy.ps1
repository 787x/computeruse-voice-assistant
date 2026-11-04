param(
    [Parameter(Mandatory=$true)][string]$Source,
    [string]$DataDir = $(if ($env:CUAVA_DATA_DIR) { $env:CUAVA_DATA_DIR } else { Join-Path $env:APPDATA 'cuava' }),
    [switch]$Apply
)
$ErrorActionPreference = 'Stop'
# Whitelist only. Never decrypt, print or copy source credential fields.
$old = Get-Content -LiteralPath $Source -Raw -Encoding UTF8 | ConvertFrom-Json
$target = Join-Path $DataDir 'settings.json'
$current = if (Test-Path -LiteralPath $target) { Get-Content -LiteralPath $target -Raw -Encoding UTF8 | ConvertFrom-Json } else { [pscustomobject]@{} }
$mapping = [ordered]@{providerId='providerId'; baseURL='baseUrl'; model='model'; contextWindow='contextWindow'; contextCompressPercent='contextCompressPercent'; systemPrompt='systemPrompt'}
$changes = [ordered]@{}
$providerTupleSafe = $old.providerId -in @('mimo','deepseek','openai-compatible')
foreach ($field in @('providerId','baseUrl','model')) {
    if ($null -ne $current.PSObject.Properties[$field]) { $providerTupleSafe = $false }
}
foreach ($name in $mapping.Keys) {
    $dest = $mapping[$name]
    if ($name -in @('providerId','baseURL','model') -and -not $providerTupleSafe) { Write-Host "保留目标提供商配置组：$dest"; continue }
    if ($null -eq $old.PSObject.Properties[$name]) { continue }
    if ($null -ne $current.PSObject.Properties[$dest]) { Write-Host "保留已有字段：$dest"; continue }
    $value = $old.$name
    if ($name -eq 'providerId' -and $value -notin @('mimo','deepseek','openai-compatible')) { Write-Host '不支持的提供商，跳过'; continue }
    if ($name -eq 'contextWindow' -and ($value -lt 1024 -or $value -gt 10000000)) { throw '上下文长度超出可导入范围' }
    if ($name -eq 'contextCompressPercent' -and ($value -lt 1 -or $value -gt 100)) { throw '压缩比例无效' }
    if ($name -eq 'baseURL') {
        $uri = $null
        if (-not [uri]::TryCreate([string]$value, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -notin @('http','https') -or $uri.UserInfo -or $uri.Query -or $uri.Fragment) { Write-Host '跳过非标准端点（不复制嵌入式凭据）'; continue }
    }
    $changes[$dest] = $value
    # Preview without exposing arbitrary strings which may embed secrets.
    if ($dest -eq 'systemPrompt') {
        Write-Host "待导入：systemPrompt（$(([string]$value).Length) 字符；正文请在源文件核对，不进入日志）"
    } else { Write-Host "待导入：$name -> $dest = $value" }
}
Write-Host '不迁移：所有 Key/enc:、Session、语音/模型文件、权限、方案/时段、音乐/搜索/角色设置。Key 请按提供商重新输入。'
if (-not $Apply -or $changes.Count -eq 0) { Write-Host '预览结束，未写入。加 -Apply 并在确认后写入。'; exit 0 }
Write-Host '请先退出 Rust 程序，避免在途保存覆盖配置。不会停止任何进程。'
if ((Read-Host '确认导入上述缺失字段请输入 IMPORT') -cne 'IMPORT') { exit 0 }
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
if (Test-Path -LiteralPath $target) { Copy-Item -LiteralPath $target -Destination "$target.pr9-$([guid]::NewGuid().ToString('N')).bak" }
foreach ($name in $changes.Keys) { $current | Add-Member -NotePropertyName $name -NotePropertyValue $changes[$name] }
$temp = Join-Path $DataDir "import-$([guid]::NewGuid().ToString('N')).tmp"
[IO.File]::WriteAllText($temp, ($current | ConvertTo-Json -Depth 100), (New-Object Text.UTF8Encoding($false)))
Move-Item -LiteralPath $temp -Destination $target -Force
Write-Host '导入完成。重复导入保留已存在字段，不触碰 Session/Key。'
