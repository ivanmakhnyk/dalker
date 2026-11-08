# Builds dist\Dalker-<version>-x64.msi with WiX 3.x.
#   powershell -File packaging\windows\build-msi.ps1 [-Exe target\static\release\dalkerd.exe]
# dalkerd-console.exe from the same folder is shipped as dalkerd.com (the console front end).
# The exe should be a static-CRT release build:
#   $env:RUSTFLAGS="-C target-feature=+crt-static"; cargo build --release -p dalker-agent --target-dir target\static
param(
    [string]$Exe = "target\static\release\dalkerd.exe",
    [string]$Wix = "${env:ProgramFiles(x86)}\WiX Toolset v3.14\bin"
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $root

# The single source of the version: [workspace.package] in Cargo.toml (see scripts/version.sh).
$version = $null; $inPkg = $false
foreach ($line in Get-Content Cargo.toml) {
    if ($line -match '^\[') { $inPkg = $line.Trim() -eq '[workspace.package]' }
    elseif ($inPkg -and $line -match '^version\s*=\s*"([^"]+)"') { $version = $Matches[1]; break }
}
if (-not $version) { throw "no version in [workspace.package] of Cargo.toml" }
# MSI product versions are numeric only: 1.2.0-rc.1 installs as 1.2.0 (the file name keeps the suffix).
$msiVersion = ($version -split '[-+]')[0]
$build = Join-Path $root "dist\msi-build"
Remove-Item -Recurse -Force $build -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $build | Out-Null

Copy-Item $Exe (Join-Path $build "dalkerd.exe")
Copy-Item (Join-Path (Split-Path $Exe) "dalkerd-console.exe") (Join-Path $build "dalkerd.com")

$exts = @("-ext", "WixUtilExtension", "-ext", "WixFirewallExtension", "-ext", "WixUIExtension")
& "$Wix\candle.exe" -nologo -arch x64 "-dVersion=$msiVersion" "-dBinDir=$build" `
    "-dIconFile=$root\assets\branding\dalker.ico" @exts `
    -out "$build\dalker.wixobj" (Join-Path $PSScriptRoot "dalker.wxs")
if ($LASTEXITCODE) { throw "candle failed" }

$msi = Join-Path $root "dist\Dalker-$version-x64.msi"
# ICE43/ICE57 flag the Start menu shortcut as per-user data; the package is perMachine, so
# ProgramMenuFolder is the all-users Start menu.
& "$Wix\light.exe" -nologo -sice:ICE61 -sice:ICE43 -sice:ICE57 @exts -out $msi "$build\dalker.wixobj"
if ($LASTEXITCODE) { throw "light failed" }
Write-Host "built $msi"
