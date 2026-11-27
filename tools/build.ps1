# Build QT DAW. Puts the WinLibs MinGW toolchain (dlltool, g++, libclang) on PATH first,
# because the Rust windows-gnu toolchain here cannot link newer Windows crates or build ASIO
# without it.
#
#   powershell -ExecutionPolicy Bypass -File tools\build.ps1            # debug, with ASIO
#   powershell -ExecutionPolicy Bypass -File tools\build.ps1 -Release
#   powershell -ExecutionPolicy Bypass -File tools\build.ps1 -NoAsio
#   powershell -ExecutionPolicy Bypass -File tools\build.ps1 -Test
param(
    [switch]$Release,
    [switch]$NoAsio,
    [switch]$Test
)
$ErrorActionPreference = 'Stop'

$mingw = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Directory -Filter 'BrechtSanders.WinLibs.POSIX.MSVCRT.LLVM*' -ErrorAction SilentlyContinue |
    ForEach-Object { Join-Path $_.FullName 'mingw64\bin' } |
    Where-Object { Test-Path (Join-Path $_ 'dlltool.exe') } |
    Select-Object -First 1
if (-not $mingw) {
    throw 'WinLibs MinGW not found. Install it with: winget install --id BrechtSanders.WinLibs.POSIX.MSVCRT.LLVM --exact --scope user'
}
$env:PATH = "$mingw;$env:PATH"
$env:LIBCLANG_PATH = $mingw

Set-Location (Split-Path $PSScriptRoot -Parent)

$cargoArgs = @()
if ($Release) { $cargoArgs += '--release' }
if (-not $NoAsio) { $cargoArgs += @('--features', 'qtdaw-probe/asio,qtdaw-app/asio') }

& cargo build --workspace @cargoArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Claude runs its own copy of the MCP connector (so it never locks the build output).
# Refresh that copy if it's installed; if Claude is using it right now, keep the old one.
$mcpCopy = Join-Path $env:LOCALAPPDATA 'QT DAW\qtdaw-mcp.exe'
$mcpBuilt = Join-Path (Get-Location) ("target\{0}\qtdaw-mcp.exe" -f $(if ($Release) { 'release' } else { 'debug' }))
if ((Test-Path $mcpCopy) -and (Test-Path $mcpBuilt) -and
    ((Get-FileHash $mcpCopy).Hash -ne (Get-FileHash $mcpBuilt).Hash)) {
    try { Copy-Item $mcpBuilt $mcpCopy -Force -ErrorAction Stop; Write-Host "Updated $mcpCopy" }
    catch { Write-Warning "Claude is using $mcpCopy; quit Claude and build again to update it." }
}

if ($Test) {
    & cargo test --workspace @cargoArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
