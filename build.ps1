# Builds the shell, the settings app and the installer that carries both:
# dist\CustomUI-Setup-<version>.exe
Set-Location $PSScriptRoot

cargo build --release --bin custom-ui --bin custom-ui-settings
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build --release --features installer --bin custom-ui-setup
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"$' | Select-Object -First 1).Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force dist | Out-Null
$setup = "dist\CustomUI-Setup-$version.exe"
Copy-Item target\release\custom-ui-setup.exe $setup -Force
Write-Host "Done: $setup"
