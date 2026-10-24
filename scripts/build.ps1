param([ValidateSet('x86','x64','aarch64','all')][string]$Architecture = 'all')
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)
$targets = @{ x86='i686-unknown-uefi'; x64='x86_64-unknown-uefi'; aarch64='aarch64-unknown-uefi' }
$names = if ($Architecture -eq 'all') { @('x86','x64','aarch64') } else { @($Architecture) }
foreach ($name in $names) {
    & cargo build --locked --release -p bootapp-efi --target $targets[$name] --bins
    if ($LASTEXITCODE -ne 0) { throw "UEFI build failed: $name" }
}
& cargo build --locked --release -p bootapp-cli
if ($LASTEXITCODE -ne 0) { throw 'CLI build failed' }
