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

Push-Location -LiteralPath $projectRoot
try {
    & $cargoExe build --locked --release --bin harbor-sql --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw "Rust 编译失败（退出码 $LASTEXITCODE）。"
    }
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
    $binary = Join-Path $targetRoot 'x86_64-pc-windows-msvc\release\harbor-sql.exe'
    $dist = Join-Path $projectRoot 'dist'
    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    $destination = Join-Path $dist 'HarborSQL.exe'
    Copy-Item -LiteralPath $binary -Destination $destination -Force
    Write-Output $destination
} finally {
    Pop-Location
}
