[CmdletBinding()]
param([string]$Executable = "$PSScriptRoot/../target/release/audio-matrix.exe", [int]$SoakSeconds = 10, [switch]$Hardware)
# Isolated engine: fake by default, or explicitly muted physical streams with
# -Hardware. Neither mode restores/writes checkpoints or changes startup.
$ErrorActionPreference = 'Stop'
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$instance = 'smoke-' + [Guid]::NewGuid().ToString('N')
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
$pipeName = "AudioMatrix-v2-$sid-$sessionId-$instance"
$modeArguments = if ($Hardware) { @() } else { @('--test-engine') }
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class EngineSmokeWindow {
    public delegate bool Callback(IntPtr hwnd, IntPtr unused);
    [DllImport("user32.dll")] public static extern bool EnumWindows(Callback callback, IntPtr unused);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, System.Text.StringBuilder title, int length);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int mode);
    public static IntPtr Find(uint pid, string expected) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, unused) => {
            uint actual; GetWindowThreadProcessId(hwnd,out actual);
            var title = new System.Text.StringBuilder(256); GetWindowText(hwnd,title,title.Capacity);
            if (actual == pid && title.ToString() == expected) { found = hwnd; return false; }
            return true;
        },IntPtr.Zero);
        return found;
    }
}
'@
function Read-Exact($pipe, [int]$count) {
    $buffer = New-Object byte[] $count
    $offset = 0
    while ($offset -lt $count) {
        $reading = $pipe.ReadAsync($buffer, $offset, $count - $offset)
        if (-not $reading.Wait(3000)) { throw 'IPC response timed out.' }
        $length = $reading.Result
        if ($length -eq 0) { throw 'IPC connection closed.' }
        $offset += $length
    }
    return ,$buffer
}
function Invoke-Engine($request, [int]$Version = 1) {
    $pipe = New-Object IO.Pipes.NamedPipeClientStream('.', $pipeName, [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
    try {
        $pipe.Connect(2000)
        $json = @{version=$Version;request=$request} | ConvertTo-Json -Compress -Depth 100
        Write-Verbose "Pipe $pipeName request $json"
        $bytes = [Text.Encoding]::UTF8.GetBytes($json)
        $header = [BitConverter]::GetBytes([uint32]$bytes.Length)
        Write-Verbose "Frame bytes $($bytes.Length), header $([BitConverter]::ToString($header))"
        $pipe.Write($header,0,4); $pipe.Write($bytes,0,$bytes.Length); $pipe.Flush()
        $header = Read-Exact $pipe 4
        $length = [BitConverter]::ToUInt32($header,0)
        if ($length -eq 0 -or $length -gt 8MB) { throw 'Invalid response size.' }
        $body = Read-Exact $pipe $length
        $pipe.WriteByte(0xA5); $pipe.Flush()
        $reply = [Text.Encoding]::UTF8.GetString($body) | ConvertFrom-Json
        if ($reply.error) { throw $reply.error }
        if ($reply.version -ne 1 -or $reply.snapshot.version -ne 1) { throw 'Unexpected protocol.' }
        return $reply.snapshot
    } finally { $pipe.Dispose() }
}
function Wait-State([string]$state) {
    $deadline = (Get-Date).AddSeconds(8)
    do {
        $snapshot = Invoke-Engine 'Status'
        if ($snapshot.state -eq $state) { return $snapshot }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    throw "Expected $state, received $($snapshot.state): $($snapshot.message)"
}
function Start-Console {
    $process = Start-Process -FilePath $Executable -ArgumentList (@('--instance',$instance) + $modeArguments) -WindowStyle Hidden -PassThru
    $process.Handle | Out-Null
    $deadline = (Get-Date).AddSeconds(12)
    do {
        $window = [EngineSmokeWindow]::Find($process.Id,'Audio Matrix')
        if ($window -ne 0) { Start-Sleep -Milliseconds 700; return $process }
        $process.Refresh()
        if ($process.HasExited) { throw "Console exited early: $($process.ExitCode)" }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    throw 'Console did not initialize.'
}
function Assert-Mix($snapshot, $expected) {
    $actual = @($snapshot.session.preset,$snapshot.audition) | ConvertTo-Json -Compress -Depth 100
    if ($actual -ne $expected) { throw 'Routing/calibration/audition changed unexpectedly.' }
    if ($Hardware -and @($snapshot.outputs | Where-Object { $_.stats.clipped -ne 0 -or @($_.stats.peaks | Where-Object { $_ -ne 0 }).Count -gt 0 }).Count -gt 0) { throw 'Hardware test must remain silent.' }
}
$engine = $null; $console = $null; $duplicate = $null
try {
    $engine = Start-Process -FilePath $Executable -ArgumentList (@('--engine','--instance',$instance) + $modeArguments) -WindowStyle Hidden -RedirectStandardError (Join-Path $env:TEMP "$instance-stderr.log") -PassThru
    $engine.Handle | Out-Null
    $deadline = (Get-Date).AddSeconds(8)
    do {
        try { $snapshot = Invoke-Engine 'Status'; break } catch { $lastFailure = $_; Start-Sleep -Milliseconds 100 }
    } while ((Get-Date) -lt $deadline)
    if (-not $snapshot -or $snapshot.test_mode -eq [bool]$Hardware) { Get-Content -Encoding UTF8 (Join-Path $env:TEMP "$instance-stderr.log") | Write-Warning; throw "Isolated engine unavailable: $lastFailure" }
    $engineId = $snapshot.engine_pid
    if ($engineId -ne $engine.Id) { throw 'Unexpected engine process.' }
    $duplicate = Start-Process -FilePath $Executable -ArgumentList (@('--engine','--instance',$instance) + $modeArguments) -WindowStyle Hidden -PassThru
    if (-not $duplicate.WaitForExit(5000) -or $duplicate.ExitCode -ne 0) { throw 'Duplicate engine was not rejected.' }
    # Configure before attaching the console; fake capture source and two outputs.
    $session = @'
{"version":1,"preferences":{"restore_session":true,"auto_start_audio":false,"start_with_windows":false,"close_to_tray":false,"spectrum":true,"recover_input":true},"preset_path":"isolated-calibration.json","preset":{"version":1,"source_mode":"Capture","source_id":"test-input","latency_ms":30,"main_clock_id":"test-a","outputs":[]},"scenes":[],"source_format":null}
'@ | ConvertFrom-Json
    $session.source_format = @{Endpoint=$snapshot.devices[2]}
    $selected = @($snapshot.devices | Where-Object { $_.kind -eq 'Playback' })
    if ($Hardware) {
        $realtek = $selected | Where-Object { $_.name.Contains('Realtek') -and -not $_.name.Contains('Digital') } | Select-Object -First 1
        $monitor = $selected | Where-Object { $_.name.Contains('LG ULTRAGEAR') } | Select-Object -First 1
        $selected = @(@($realtek,$monitor) | Where-Object { $null -ne $_ })
        if ($selected.Count -ne 2 -or @($selected | Where-Object { $_.channels -ne 2 }).Count -ne 0) { throw 'Muted hardware test requires stereo Realtek + LG outputs.' }
        $session.preset.source_mode = 'Tone'; $session.preset.source_id = ''; $session.source_format = 'Tone'
        $session.preset | Add-Member -NotePropertyName global_mute -NotePropertyValue $true
    }
    $session.preset.main_clock_id = $selected[0].id
    $session.preset.outputs = $selected | ForEach-Object {
        @{device_id=$_.id;enabled=$true;delay_ms=127.0;gain_db=-12.0;mute=[bool]$Hardware;eq=@();channels=@(
            @{routes=@(1.0,0.0);gain_db=0.0;mute=$false;invert=$false;eq=@()},
            @{routes=@(0.0,1.0);gain_db=-2.0;mute=$false;invert=$true;eq=@()})}
    }
    $update = @{session=$session;audition=@{devices=@($selected[0].id);channels=@()};base_gain=$null;base_mute=$null}
    $snapshot = Invoke-Engine @{Start=$update}
    $snapshot = Wait-State 'Running'
    $originalDevices = $snapshot.devices
    $console = Start-Console
    $snapshot = Invoke-Engine 'Status'
    $expected = @($snapshot.session.preset,$snapshot.audition) | ConvertTo-Json -Compress -Depth 100
    if ($snapshot.engine_pid -eq $console.Id -or -not $snapshot.live) { throw 'Console owns the engine or routing stopped.' }
    $window = [EngineSmokeWindow]::Find($console.Id,'Audio Matrix')
    [EngineSmokeWindow]::ShowWindow($window,0) | Out-Null
    $duplicate = Start-Process -FilePath $Executable -ArgumentList (@('--instance',$instance) + $modeArguments) -WindowStyle Hidden -PassThru
    if (-not $duplicate.WaitForExit(5000) -or $duplicate.ExitCode -ne 0) { throw 'Duplicate console was not rejected.' }
    Start-Sleep -Milliseconds 500
    if (-not [EngineSmokeWindow]::IsWindowVisible($window)) { throw 'Second launch did not restore the existing console.' }
    $before = (Invoke-Engine 'Status').outputs[0].stats.frames_written
    [EngineSmokeWindow]::PostMessage($window,0x10,0,0) | Out-Null
    if (-not $console.WaitForExit(8000) -or $console.ExitCode -ne 0) { throw 'Console did not close cleanly.' }
    Start-Sleep -Milliseconds 200
    $snapshot = Invoke-Engine 'Status'
    Assert-Mix $snapshot $expected
    if (-not $snapshot.live -or $snapshot.engine_pid -ne $engineId -or $snapshot.outputs[0].stats.frames_written -le $before) { throw 'Closing console interrupted the engine.' }
    $console = Start-Console
    $before = (Invoke-Engine 'Status').outputs[0].stats.frames_written
    Stop-Process -Id $console.Id
    $console.WaitForExit()
    Start-Sleep -Milliseconds 200
    $snapshot = Invoke-Engine 'Status'
    Assert-Mix $snapshot $expected
    if (-not $snapshot.live -or $snapshot.outputs[0].stats.frames_written -le $before) { throw 'Forced console termination stopped the engine.' }
    # Synthetic input recovery is restricted to fake backends.
    if (-not $Hardware) {
        Invoke-Engine @{TestDevices=@($originalDevices[0],$originalDevices[1])} | Out-Null
        Wait-State 'Recovering' | Out-Null
        Invoke-Engine @{TestDevices=$originalDevices} | Out-Null
        $snapshot = Wait-State 'Running'; Assert-Mix $snapshot $expected
    }
    # Exercise the actual tray WindowProc power-message path; never suspend Windows.
    $tray = [EngineSmokeWindow]::Find($engineId,'Audio Matrix tray')
    if ($tray -eq 0) { throw 'Engine tray owner missing.' }
    [EngineSmokeWindow]::PostMessage($tray,0x218,4,0) | Out-Null
    Wait-State 'Suspended' | Out-Null
    foreach ($resume in 18,7) { [EngineSmokeWindow]::PostMessage($tray,0x218,$resume,0) | Out-Null }
    $snapshot = Wait-State 'Running'; Assert-Mix $snapshot $expected
    if (-not $Hardware) {
        Invoke-Engine @{TestDevices=@($originalDevices[1],$originalDevices[2])} | Out-Null
        if ((Invoke-Engine 'Status').outputs.Count -ne 1) { throw 'Offline output not retired independently.' }
        Invoke-Engine @{TestDevices=$originalDevices} | Out-Null
        # A changed native input format must not be silently accepted.
        $changed = @($originalDevices | ForEach-Object { $_ | ConvertTo-Json -Depth 100 | ConvertFrom-Json })
        $changed[2].sample_rate = 44100
        Invoke-Engine @{TestDevices=$changed} | Out-Null
        Wait-State 'FormatBlocked' | Out-Null
        Invoke-Engine @{TestDevices=$originalDevices} | Out-Null
        Wait-State 'Running' | Out-Null
    }
    $rejected = $false
    try { Invoke-Engine 'Status' -Version 999 | Out-Null } catch { $rejected = $true }
    if (-not $rejected) { throw 'Unknown protocol version accepted.' }
    $deadline = (Get-Date).AddSeconds([Math]::Max(1,$SoakSeconds))
    do {
        $snapshot = Invoke-Engine 'Status'
        Assert-Mix $snapshot $expected
        if (-not $snapshot.live -or $snapshot.engine_pid -ne $engineId) { throw 'Engine soak failed.' }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    if ($Hardware) {
        if (@($snapshot.outputs | Where-Object { $_.stats.budget_ms -le 0 -or -not $_.stats.realtime_scheduled -or $_.stats.spectrum.Count -ne 64 }).Count -gt 0) { throw 'Missing hardware processing/scheduling/FFT diagnostics.' }
        $snapshot.outputs | ForEach-Object { Write-Output "Hardware: $($_.device_id), frames=$($_.stats.frames_written), block-p95=$($_.stats.budget_p95_percent)%, misses=$($_.stats.deadline_misses), underruns=$($_.stats.underruns), FFT-dropped=$($_.stats.analysis_dropped), MMCSS=$($_.stats.realtime_scheduled)" }
    }
    $console = Start-Console
    Assert-Mix (Invoke-Engine 'Status') $expected
    # Tray controls stay authoritative while the console is hidden; unrelated
    # UI configuration/checkpoint traffic must not revert them.
    $window = [EngineSmokeWindow]::Find($console.Id,'Audio Matrix')
    [EngineSmokeWindow]::ShowWindow($window,0) | Out-Null
    $prior = Invoke-Engine 'Status'
    [EngineSmokeWindow]::PostMessage($tray,0x111,2,0) | Out-Null
    [EngineSmokeWindow]::PostMessage($tray,0x111,4,0) | Out-Null
    $deadline = (Get-Date).AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 100
        $snapshot = Invoke-Engine 'Status'
    } while (($snapshot.session.preset.global_mute -eq $prior.session.preset.global_mute -or $snapshot.session.preset.global_gain_db -ne $prior.session.preset.global_gain_db - 1) -and (Get-Date) -lt $deadline)
    if ($snapshot.session.preset.global_mute -eq $prior.session.preset.global_mute -or $snapshot.session.preset.global_gain_db -ne $prior.session.preset.global_gain_db - 1) { throw 'Hidden tray controls were ignored or reverted.' }
    $expected = @($snapshot.session.preset,$snapshot.audition) | ConvertTo-Json -Compress -Depth 100
    Start-Sleep -Milliseconds 400
    Assert-Mix (Invoke-Engine 'Status') $expected
    [EngineSmokeWindow]::PostMessage($tray,0x111,1,0) | Out-Null
    Start-Sleep -Milliseconds 400
    if (-not [EngineSmokeWindow]::IsWindowVisible($window)) { throw 'Tray Show did not restore the attached console.' }
    [EngineSmokeWindow]::PostMessage($tray,0x111,6,0) | Out-Null
    $exited = $engine.WaitForExit(10000)
    $engine.Refresh()
    if (-not $exited -or $engine.ExitCode -ne 0) { throw "Engine tray Exit did not stop cleanly: exited=$exited, code=$($engine.ExitCode), pid=$engineId" }
    [EngineSmokeWindow]::PostMessage([EngineSmokeWindow]::Find($console.Id,'Audio Matrix'),0x10,0,0) | Out-Null
    if (-not $console.WaitForExit(8000)) { throw 'Disconnected console did not close.' }
    $backend = if ($Hardware) { 'muted physical WASAPI streams' } else { 'no-audio fake backend with input/output/format recovery' }
    Write-Output "PASS: single-instance, real IPC, UI close/crash/reattach, retained calibration/solo, synthetic Windows power events, protocol rejection, $SoakSeconds-second soak, tray Exit ($backend). No checkpoint/registry writes."
} catch {
    Get-Content -Encoding UTF8 (Join-Path $env:TEMP "$instance-stderr.log") | Select-Object -Last 12 | Write-Warning
    throw
} finally {
    # Only processes this script created; never touch any existing user instance.
    foreach ($process in @($console,$duplicate,$engine)) {
        if ($null -ne $process) {
            $process.Refresh()
            if (-not $process.HasExited) { Stop-Process -Id $process.Id }
        }
    }
    Remove-Item -LiteralPath (Join-Path $env:TEMP "$instance-stderr.log") -ErrorAction SilentlyContinue
}
