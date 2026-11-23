# Builds the release EXE, the portable ZIP and (if Inno Setup is installed) FindForge-Setup.exe.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\package.ps1
# The build goes to .\target, or to CARGO_TARGET_DIR when that is set.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
$exe = Join-Path $target "release\FindForge.exe"
if (-not (Test-Path $exe)) { throw "The build did not produce $exe" }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force $dist | Out-Null

# The build script renders the icon into OUT_DIR; copy the newest one for the installer.
$ico = Get-ChildItem (Join-Path $target "release\build") -Recurse -Filter findforge.ico | Sort-Object LastWriteTime -Descending | Select-Object -First 1
$icoCopy = Join-Path $dist "findforge.ico"
Copy-Item $ico.FullName $icoCopy -Force

$stage = Join-Path $dist "portable"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe $stage
Copy-Item LICENSE (Join-Path $stage "LICENSE.txt")
$zip = Join-Path $dist "FindForge-$version-portable-x64.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Copy-Item $exe (Join-Path $dist "FindForge.exe") -Force

$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if ($iscc) {
    & $iscc "/DAppVersion=$version" "/DAppExe=$exe" "/DAppIcon=$icoCopy" installer\FindForge.iss
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
} else {
    Write-Warning "Inno Setup 6 not found: skipping FindForge-Setup.exe (winget install JRSoftware.InnoSetup)"
}
$size = (Get-Item $exe).Length / 1MB
Write-Host ("FindForge.exe {0:N2} MB" -f $size)
Get-ChildItem $dist -File | Format-Table Name, @{n="MB";e={"{0:N2}" -f ($_.Length/1MB)}}
