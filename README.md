# Audio Matrix

A Rust desktop application for Windows that takes one multichannel source and routes it to several output devices. Each output device and each of its channels has separate volume, mute and five-band parametric EQ. The routing grid connects individual source channels to output channels; multiple connections sum, and their coefficients can be adjusted independently.

## Current status

The dependency-free DSP core is compiled and tested. The egui interface and WASAPI integration are implemented in source, but **have not been compiled or tested with audio hardware**: this development session blocks downloads from crates.io. This is a development prototype, not a ready-to-install virtual sound device.

The application's own Windows-selectable playback endpoint is **not implemented yet**. Until the driver milestone, the input can be an existing virtual cable's recording endpoint, a microphone/capture endpoint, a playback endpoint captured through loopback, or the built-in quiet test tone. No driver is installed by this project.

## Development

Requires Windows 10/11, Rust 1.88 or newer, Visual Studio C++ build tools, and the Windows SDK. This workspace's Rust installation currently uses stable 1.99.0. The first desktop build needs internet access to crates.io. Dependencies are cached under `.tools/cargo-cache` by the development script.

```powershell
# Run the application, or compile without opening it:
.\scripts\dev.ps1 run
.\scripts\dev.ps1 build -Release

# Enumerate actual active Windows endpoints without starting routing:
.\scripts\dev.ps1 devices

# Full project checks (requires dependencies):
.\scripts\dev.ps1 check
.\scripts\dev.ps1 test

# DSP tests need only rustc, with no downloads:
.\scripts\test-core.ps1 -Optimize
```

If Cargo is configured for offline use on your development machine, set `$env:CARGO_NET_OFFLINE = 'false'` in a terminal with network access before the first build. A restricted session's network proxy still prevents downloads; changing Cargo's offline setting alone cannot fix that.

## Using the prototype once built

1. Select an input. For a virtual cable, configure your application to play to the cable's playback endpoint and select the cable's capture endpoint in **Capture device / virtual cable**. Loopback captures applications already playing to the selected output and does not silence the original playback.
2. Enable the physical outputs you want. Select an output device to edit its routing and processing. Every device starts at -12 dB; surround outputs beyond matching input channels start disconnected.
3. Connect channels in the routing grid. Configure individual mix coefficients under **Source mix coefficients**. For a stereo-to-mono mix, use 0.5 for each source if you want headroom for correlated signals.
4. Adjust device volume/EQ, then select an output channel to adjust its volume, polarity and EQ. Changes take effect while routing. Input selection, enabled devices and buffering settings require stopping first.
5. Start routing and watch the output meters and buffer counters. A quiet 440 Hz test tone is available for a hardware smoke test. Stop immediately if the selected physical setup feeds a microphone back through speakers.
6. Save or load a JSON preset using the path at the bottom of the window. Presets identify devices by Windows endpoint IDs. They never start audio automatically.

The router rejects capturing a playback endpoint and rendering back into that same endpoint. EQ boosts and summed routes can exceed full scale: leave headroom and monitor the clipping counter. Output samples are hard-clamped to [-1, 1]; this is protection, not a dynamics limiter.

## Architecture

```text
Windows apps -> virtual cable / future Matrix Input driver
                               |
                      WASAPI capture worker
                               |
                 bounded per-output source queues
                      /                    \
              output worker A        output worker B
              clock correction       clock correction
              channel matrix         channel matrix
              channel EQ             channel EQ
              device EQ              device EQ
              channel/device gain    channel/device gain
              meter + clamp          meter + clamp
              WASAPI render          WASAPI render
                    |                      |
              physical device A      physical device B
```

- `crates/matrix-core`: dependency-free routing, biquad EQ, smoothed gains/matrix coefficients, clipping accounting and bounded audio queues. EQ uses peaking filters; frequency, gain and Q are editable.
- `src/audio`: WASAPI backend. COM audio objects are created and used on their owning worker thread. Capture retains the source's native channel layout and format; outputs request float audio at the source rate with the device's channel mask. Windows shared-mode conversion handles nominal sample-rate differences.
- `src/app.rs`: egui desktop interface, source/output selection, channel matrix, EQ response plots and meters.
- `src/preset.rs`: versioned JSON, pre-application validation and replacement through a temporary file.

Device volume adjusts this router's stream, not Windows' global endpoint volume. Output channels correspond to the channels exposed by the device's Windows shared-mode configuration. Configure the device for 5.1/7.1 in Windows first if you need those channels. Channel labels use the endpoint's speaker mask where available.

The queues use a small adaptive linear interpolation adjustment (limited to ±2000 ppm) to follow separate hardware clocks. This is an initial drift controller, not sample-accurate synchronization or a high-quality general resampler. Locks are brief but this MVP is not a hard real-time engine. Buffer target is not measured end-to-end latency. EQ parameter changes currently update coefficients at block boundaries; gain/mute/routing changes are smoothed over roughly 10 ms. A device failure stops routing and reports an error; reconnect and refresh to restart.

## Next milestones

1. Restore Cargo dependencies, compile the desktop app, and validate capture, repeated start/stop, live controls, multichannel routing and playback to two actual devices over an extended run.
2. Replace the initial drift interpolator with a band-limited asynchronous resampler, improve parameter interpolation and add per-output delay/alignment controls and a proper limiter.
3. Implement **Matrix Input**, a real virtual render endpoint that other Windows applications can select. Its kernel driver will accept PCM independently of the UI and transfer it through a versioned transport to the Rust engine. Start with stereo/48 kHz, then add multichannel formats and negotiate the source layout.
4. Add driver packaging/signing, device reconnection handling, a background routing service and installer. ASIO/exclusive mode and multiple simultaneous sources are later expansions.

Driver reference: Microsoft's [SysVAD sample](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad) illustrates Windows virtual audio endpoints but does not implement the transport needed here. [Windows driver signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-) applies to distributing the kernel driver. Backend API reference: [wasapi-rs](https://github.com/HEnquist/wasapi-rs).
