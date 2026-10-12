param(
    [ValidateSet('run', 'build', 'check', 'test', 'lint', 'devices', 'test-hardware', 'test-native', 'preview')][string]$Task = 'run',
    [switch]$Release,
    [string]$Target = ''
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
if ($cargoCommand) { $cargoPath = $cargoCommand.Source }
else { $cargoPath = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
if (-not (Test-Path -LiteralPath $cargoPath)) { throw 'Install Rust using rustup first.' }
$originalCargoHome = $env:CARGO_HOME
$originalPath = $env:PATH
try {
    # Keep downloaded dependencies within this project for restricted workspaces.
    $env:CARGO_HOME = Join-Path $projectRoot '.tools\cargo-cache'
    $env:PATH = (Split-Path -Parent $cargoPath) + ';' + $env:PATH
    Push-Location -LiteralPath $projectRoot
    try {
        [string[]]$arguments = @(switch ($Task) {
            'run' { @('run') }
            'build' { @('build') }
            'check' { @('check', '--workspace') }
            'test' { @('test', '--workspace') }
            'lint' { @('clippy', '--workspace', '--all-targets') }
            'devices' { @('run') }
            'preview' { @('run') }
            'test-hardware' { @('test', 'hardware_live_output_switching') }
            'test-native' { @('test', 'native_') }
        })
        if ($Release) { $arguments += '--release' }
        if ($Target) { $arguments += @('--target', $Target) }
        if ($Task -eq 'lint') { $arguments += @('--', '-D', 'warnings') }
        if ($Task -eq 'devices') { $arguments += @('--', '--list-devices') }
        if ($Task -eq 'preview') { $arguments += @('--', '--preview-board') }
        if ($Task -eq 'test-hardware') { $arguments += @('--', '--ignored', '--nocapture', '--test-threads=1') }
        if ($Task -eq 'test-native') { $arguments += @('--', '--ignored', '--nocapture') }
        & $cargoPath @arguments
        $commandExitCode = $LASTEXITCODE
    } finally { Pop-Location }
} finally {
    $env:CARGO_HOME = $originalCargoHome
    $env:PATH = $originalPath
}
exit $commandExitCode

