# Audio Matrix

One audio source. Multiple outputs. Your mix.

Audio Matrix is a Windows audio router with a dark, single-page soundboard. Send one source to multiple playback devices, choose which channels go where, and adjust each device and channel independently.

**Early preview:** Audio Matrix does not yet create its own Windows playback device. Use an existing virtual audio cable, capture playback from a Windows output, or select a recording device as the input. No virtual audio driver or installer is included.

## Features

- Play through multiple Windows output devices at once; switch outputs and edit their settings while audio is running.
- Route individual input channels using a matrix inside each output's card.
- Control global, device and channel volume, mute, solo and channel polarity. Link stereo controls when needed.
- Shape sound with device/channel EQ, draggable curves, knobs, built-in and custom presets, Copy/Paste and A/B comparisons.
- Protect outputs with a linked look-ahead limiter and monitor levels, gain reduction and engine health.
- Align outputs using automatic Windows stream timing, a selectable main clock and manual delay adjustments.
- Save routing presets, recall named scenes, and undo or redo mix changes.
- Keep routing in the background after closing the console, with tray controls and automatic device recovery.

## Getting started

Requires Windows 10 or 11 and output devices available to Windows audio. Launch `audio-matrix.exe` from your Windows build, or follow [Build from source](#build-from-source).

1. Choose an **Input** in the left sidebar:

   - **Capture device / virtual cable:** send your application's audio to an existing virtual cable's playback device, then select that cable's recording device here. You can also select a microphone or another recording device.
   - **Capture playback (loopback):** capture sound already playing through a selected Windows output. This does not silence the original output; route to different devices to avoid duplicate playback.
   - **Test tone:** check your routing with the built-in quiet tone. It becomes audible when routing starts.

2. Enable the devices you want under **Output devices**. Each enabled output appears as a card on the same page.
3. Check the channel connections in each card's routing matrix. Start with low volume and leave the limiter enabled.
4. Click **Start routing**. Adjust faders, EQ, channel connections and output switches live.
5. Use **Save** or **Save as…** to keep your configuration as a JSON preset. Use **Open…** to load one while routing is stopped.

Input selection, buffer settings and preset loading require stopping routing. Device and channel controls remain editable while running. The volume controls affect Audio Matrix's streams, not Windows' system-wide device volume.

If you use a microphone, avoid feeding it back into nearby speakers.

## Keeping outputs in sync

Leave **Auto-align outputs** enabled and allow a few seconds for measurements to settle. Select **Main clock** on one enabled output if you want that device to be the timing reference. Use **Re-align** to refresh measurements without stopping audio.

If you still hear an echo, increase **Manual delay** on the faster-sounding output. Automatic alignment measures Windows stream timing, not sound arriving at your ears: processing inside monitors, speakers or Bluetooth devices can leave a residual delay. Manual adjustments are saved with your preset.

## Background audio and the tray

Closing the console **does not stop audio**. Routing runs in a separate background engine; opening Audio Matrix again reconnects to it. A second launch restores the existing console instead of starting another mix.

- **Stop routing** stops streams and cancels pending recovery, but leaves the app available.
- **App → Exit and stop audio**, or **Exit** in the tray menu, shuts down the background engine.
- The tray menu also offers Show, mute, volume adjustments and start/stop.
- **App → Close window to tray** hides the console instead of closing it.

The background engine runs under your Windows account. It is not an installed Windows service and does not keep playing after sign-out or an engine-process crash.

## Presets, scenes and preferences

**Presets** save your routing, gains, EQ, delays, protection settings and device layout to a JSON file. **Scenes** recall named mixes without replacing your current delay, clock or limiter calibration.

Audio Matrix remembers the board and preferences separately in `%LOCALAPPDATA%\AudioMatrix\session.json`; this does not automatically overwrite your preset files. Temporary solo selections are not saved to disk.

Under **App**, session restore is enabled by default. Automatic playback after restoring and running at Windows sign-in are separate, optional settings, both off by default. Live spectrum analysis is also optional.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| Ctrl+S | Save preset |
| Ctrl+Shift+S | Save preset as… |
| Ctrl+Z | Undo |
| Ctrl+Y / Ctrl+Shift+Z | Redo |
| Ctrl+M | Toggle global mute |
| M / S | Mute / solo the selected device or channel |

Select a device heading or a channel's **Select** control before using M or S. These shortcuts are ignored while typing in a field. Hold Shift for finer EQ knob or curve adjustments; double-click a knob to reset it.

## Troubleshooting

- **No sound:** check that the input is receiving audio, the intended outputs are enabled, their channels are connected, and global/device/channel mute or solo is not excluding them.
- **RECOVERING:** reconnect the selected input. The app waits for that exact device rather than silently switching sources. Stop routing first if you want to choose another input. Automatic input recovery can be disabled under **App**.
- **FORMAT CHANGED:** stop routing, refresh devices, check the input and channel mapping, then restart. An output with a changed speaker layout may need to be explicitly re-enabled.
- **CONTROL LOST:** use **App → Reconnect engine control** or the engine's tray menu. Audio may still be running even when the console cannot communicate with it.
- **Persistent limiter warnings:** lower device/global volume, reduce EQ boosts or reduce summed channel levels. Protection cannot repair audio that was already clipped before reaching the router.
- **Dropouts:** try a larger **Buffer target** while routing is stopped and check the engine-health counters. Disabling the optional live spectrum can reduce analysis work.

## Preview limitations

- One input source at a time, using Windows shared-mode audio. ASIO and exclusive mode are not supported.
- Available output channels depend on the device's Windows speaker configuration; select the appropriate stereo or surround layout in Windows first.
- Automatic alignment cannot guarantee acoustic or sample-accurate synchronization between speakers.
- The limiter adds a fixed approximately 5 ms processing delay, even when bypassed. Optional 4x inter-sample protection is conservative, not certified true-peak metering.
- Recovery and background operation have automated and muted two-output checks. Actual sleep/wake, physical unplug/replug, surround hardware and long listening sessions still need broader testing.

## Build from source

Install Rust 1.88 or newer with the Windows MSVC toolchain, Visual Studio C++ build tools and the Windows SDK. The first build needs internet access to download dependencies.

From the repository folder in PowerShell:

```powershell
.\scripts\dev.ps1 -Task build -Release
.\target\release\audio-matrix.exe
```

To run the development version:

```powershell
.\scripts\dev.ps1 -Task run
```

Basic development checks:

```powershell
.\scripts\dev.ps1 -Task test
.\scripts\dev.ps1 -Task lint
```

When updating from an older preview, fully exit the old version before launching the new executable.
