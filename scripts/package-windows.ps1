$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$cargoCommand = Get-Command cargo.exe -ErrorAction SilentlyContinue
$cargoExe = if ($cargoCommand) {
    $cargoCommand.Source
} else {
    Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo\bin\cargo.exe'
}
$cargoBinDir = Split-Path -Parent $cargoExe
if ($cargoBinDir -and (($env:Path -split ';') -notcontains $cargoBinDir)) {
    $env:Path = "$cargoBinDir;$env:Path"
}

# 默认使用正常中等完整性的本地缓存路径作为临时构建目录（避开 claudecodeWorkSpace 的低完整性标签），
# 打包结束后无论成功还是失败都会自动删除该临时构建目录，仅保留最终产物。
$previousTargetDir = $env:CARGO_TARGET_DIR
$useManagedTempTarget = [string]::IsNullOrWhiteSpace($previousTargetDir)
$targetRoot = if ($useManagedTempTarget) {
    Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'harbor-sql-target'
} else {
    $previousTargetDir
}
$outputDir = Join-Path (Split-Path -Parent (Split-Path -Parent $projectRoot)) '安装包'

Push-Location -LiteralPath $projectRoot
try {
    if ($useManagedTempTarget) {
        $env:CARGO_TARGET_DIR = $targetRoot
    }
    & $cargoExe build --locked --release --bin harbor-sql --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw "Rust 编译失败（退出码 $LASTEXITCODE）。"
    }
    $binary = Join-Path $targetRoot 'x86_64-pc-windows-msvc\release\harbor-sql.exe'
    New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
    $destination = Join-Path $outputDir 'HarborSQL.exe'
    Copy-Item -LiteralPath $binary -Destination $destination -Force
    Write-Output $destination
} finally {
    Pop-Location
    if ($useManagedTempTarget) {
        if ($null -eq $previousTargetDir) {
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
        } else {
            $env:CARGO_TARGET_DIR = $previousTargetDir
        }
        if (Test-Path -LiteralPath $targetRoot) {
            Remove-Item -LiteralPath $targetRoot -Recurse -Force
        }
    }
}
