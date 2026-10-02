# 桌面端构建并打包（Windows 默认；macOS/Linux 由 build_desktop.sh 或 -Target 覆盖）。
# 用法：.\scripts\build_desktop.ps1 [-Target x86_64-pc-windows-msvc] [-Repo owner/repo]
param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$Repo = "librespot-org/librespot"
)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
Write-Host "Building cp-spotify-module $Version for $Target..."

cargo build --release --features jni --target $Target
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$Platform = switch -Wildcard ($Target) {
    "*windows*" { "windows" }
    "*darwin*"  { "macos" }
    "*linux*"   { "linux" }
    default     { throw "unknown target: $Target" }
}
$Entry = @{ "windows" = "cp_spotify.dll"; "macos" = "libcp_spotify.dylib"; "linux" = "libcp_spotify.so" }[$Platform]

$Stage = "./target/cp_stage"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force $Stage | Out-Null
Copy-Item "./target/$Target/release/$Entry" $Stage

$OutDir = "./target/cp_out"
New-Item -ItemType Directory -Force $OutDir | Out-Null
python scripts/pack_module.py --platform $Platform --version $Version `
    --stage $Stage --repo $Repo --out $OutDir
Write-Host "Done: $OutDir"
