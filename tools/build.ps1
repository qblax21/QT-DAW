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

if ($Test) {
    & cargo test --workspace @cargoArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
