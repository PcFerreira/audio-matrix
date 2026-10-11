param([switch]$Optimize)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$rustCompiler = Get-Command rustc -ErrorAction SilentlyContinue
if ($rustCompiler) { $compilerPath = $rustCompiler.Source }
else { $compilerPath = Join-Path $env:USERPROFILE '.cargo\bin\rustc.exe' }
if (-not (Test-Path -LiteralPath $compilerPath)) { throw 'Install Rust using rustup first.' }
$testDirectory = Join-Path $projectRoot '.tools'
New-Item -ItemType Directory -Force -Path $testDirectory | Out-Null
$testExecutable = Join-Path $testDirectory 'matrix-core-tests.exe'
$compilerArguments = @('--edition', '2024', '--test', (Join-Path $projectRoot 'crates\matrix-core\src\lib.rs'), '-o', $testExecutable)
if ($Optimize) { $compilerArguments += '-O' }
& $compilerPath @compilerArguments
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $testExecutable
exit $LASTEXITCODE

