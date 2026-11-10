# Package a release binary and 使用说明.txt into dist/.
# The MSVC CRT is statically linked via .cargo/config.toml. This script rejects
# an executable that still imports the Visual C++ redistributable or OpenH264.dll.
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("host", "client")]
    [string]$Which
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..\..")

$package = "rd-$Which"
cargo build --release --locked -p $package
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$exeName = if ($Which -eq "host") { "rd-host.exe" } else { "rd-client.exe" }
$stageName = "lowlat-desk-$Which-windows-x64"
$stage = Join-Path "dist" $stageName
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

$exe = Join-Path "target\release" $exeName
if (-not (Test-Path $exe)) { throw "missing $exe" }
Copy-Item $exe $stage

$bundled = Get-ChildItem "target\release" -Filter *.dll -File -ErrorAction SilentlyContinue
foreach ($dll in $bundled) {
    Write-Host "bundling $($dll.Name)"
    Copy-Item $dll.FullName $stage
}

$instructions = Join-Path ".github\instructions" "$Which-windows.txt"
Copy-Item $instructions (Join-Path $stage "使用说明.txt")

python .github/scripts/pe_imports.py (Join-Path $stage $exeName)
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "packaged $stage"
Get-ChildItem $stage | ForEach-Object { Write-Host "  $($_.Name)" }
