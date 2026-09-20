param([switch]$SkipFrontend)

$ErrorActionPreference = 'Stop'
# Run from Developer PowerShell for VS 2022 with the x64 MSVC toolchain.
foreach ($command in @('cargo', 'node', 'cl.exe')) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Missing $command. Use Developer PowerShell with Rust MSVC and Node.js installed."
    }
}
Push-Location $PSScriptRoot
try {
    if (-not $SkipFrontend) {
        Push-Location 'crates\dowse-app'
        try {
            & npm.cmd ci
            if ($LASTEXITCODE -ne 0) { throw 'Frontend dependency installation failed.' }
            & npm.cmd run build
            if ($LASTEXITCODE -ne 0) { throw 'Frontend build failed.' }
        } finally { Pop-Location }
    }
    if (-not (Test-Path -LiteralPath 'crates\dowse-app\build\index.html')) {
        throw 'Missing frontend assets. Run without -SkipFrontend.'
    }
    & cargo build --release --locked -p dowse
    if ($LASTEXITCODE -ne 0) { throw 'CLI build failed.' }
    New-Item -ItemType Directory -Path 'crates\dowse-app\src-tauri\binaries' -Force | Out-Null
    Copy-Item -LiteralPath 'target\release\dowse.exe' -Destination 'crates\dowse-app\src-tauri\binaries\dowse-x86_64-pc-windows-msvc.exe'
    & cargo build --release --locked -p dowse-app --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed. Close the app before rebuilding.' }
    Write-Host "Standalone app: $PSScriptRoot\target\release\dowse-app.exe"
} finally { Pop-Location }
