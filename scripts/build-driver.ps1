param([switch]$Check)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$cargoPath = Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe'
$wdkRoot = 'C:/Program Files (x86)/Windows Kits/10'
$clangRoot = 'C:/Program Files/LLVM/bin'
if (-not (Test-Path -LiteralPath $cargoPath)) { throw 'Rust is required.' }
if (-not (Test-Path -LiteralPath "$wdkRoot/Include/10.0.22621.0/km/portcls.h")) {
    throw 'Install WDK 10.0.22621 and its matching SDK before building the probe.'
}
if (-not (Test-Path -LiteralPath "$clangRoot/libclang.dll")) {
    throw 'LLVM with libclang.dll is required (tested with LLVM 17.0.6).'
}
$savedVariables = @{}
foreach ($name in @('CARGO_HOME', 'PATH', 'LIBCLANG_PATH', 'WDKContentRoot', 'WindowsSDKVersion')) {
    $savedVariables[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    $env:CARGO_HOME = Join-Path $projectRoot '.tools/cargo-cache'
    $env:PATH = (Split-Path -Parent $cargoPath) + ';' + $env:PATH
    $env:LIBCLANG_PATH = $clangRoot
    $env:WDKContentRoot = $wdkRoot + '/'
    $env:WindowsSDKVersion = '10.0.22621.0'
    Push-Location -LiteralPath (Join-Path $projectRoot 'driver/matrix-input')
    try {
        if ($Check) { & $cargoPath check --target x86_64-pc-windows-msvc }
        else { & $cargoPath build --release --target x86_64-pc-windows-msvc }
        if ($LASTEXITCODE -ne 0) { throw "Driver compilation failed ($LASTEXITCODE)." }
        if (-not $Check) {
            $driverBinary = Join-Path $PWD 'target/x86_64-pc-windows-msvc/release/matrix_input.dll'
            $probeBinary = Join-Path $PWD 'target/x86_64-pc-windows-msvc/release/matrix_input.sys'
            Copy-Item -LiteralPath $driverBinary -Destination $probeBinary
            Write-Output "Built $probeBinary"
            Write-Output 'Build-only probe. Not signed, installable or a Windows audio endpoint. Nothing was installed.'
        }
    } finally { Pop-Location }
} finally {
    foreach ($name in $savedVariables.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedVariables[$name], 'Process')
    }
}
