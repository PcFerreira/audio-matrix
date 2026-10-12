# Audio Matrix

A Rust desktop application for Windows that takes one multichannel source and routes it to several output devices. Each output device and each of its channels has separate volume, mute and five-band parametric EQ. The routing grid connects individual source channels to output channels; multiple connections sum, and their coefficients can be adjusted independently.

## Current status

The desktop app has a fixed dark theme and a single soundboard page. All enabled outputs appear together, each with device/channel faders, meters, mute, per-channel polarity/EQ, its own routing matrix, and a linked look-ahead limiter with gain-reduction metering. Global volume/mute controls every router stream while preserving the device fader settings. Device and channel EQs are always visible in the scrollable board, using frequency/gain/Q knobs instead of numeric inputs or drawers. Built-in and named custom EQ presets can be reused across devices and channels. Outputs and controls remain editable live without restarting the input or unrelated outputs.

There are 84 automated tests covering routing, clocks/alignment, UI controls, stable warning layout, offline-device preservation, solo/shortcuts, scenes, checkpoint transactions, filter shapes and smooth EQ changes, windowed-sinc interpolation, optional FFT metering, conservative 4x inter-sample protection, presets and the PCM contract. Strict Clippy and release/UI smoke checks are part of verification. Three separately invoked Windows checks cover native tray callbacks/cleanup, endpoint notification registration/enumeration, and muted Realtek analog + LG ULTRAGEAR streams (live switching, clocks, alignment, limiter/analysis toggles and failure isolation). Physical unplug/replug and long listening/soak tests still need manual validation. The user resolved their LG ULTRAWIDE delay using saved calibration; automated stream-clock measurements are not acoustic calibration.

The latest upgrade build is `target/x86_64-pc-windows-msvc/release/audio-matrix.exe`. The older `target/release/audio-matrix.exe` was running during development and has not been replaced or closed. Both calibration JSON files were left byte-for-byte unchanged; open the desired file before starting routing. Automatic playback is off by default, separately opt-in from session restore. GUI-only previews do not restore/write sessions or open streams.

Driver groundwork now includes the installed SDK/WDK 10.0.22621, LLVM 17.0.6, a separately built Rust WDM probe and a tested no_std PCM packet contract. The probe is not an audio endpoint, deliberately refuses loading, and has not been installed. See [driver development status](driver/README.md).

The application's own Windows-selectable playback endpoint is **not implemented yet**. Until the driver milestone, the input can be an existing virtual cable's recording endpoint, a microphone/capture endpoint, a playback endpoint captured through loopback, or the built-in quiet test tone. No driver is installed by this project.

## Development

Requires Windows 10/11, Rust 1.88 or newer, Visual Studio C++ build tools, and the Windows SDK. This workspace's Rust installation currently uses stable 1.99.0. The first desktop build needs internet access to crates.io. Dependencies are cached under `.tools/cargo-cache` by the development script.

```powershell
# Run the application, or compile without opening it:
.\scripts\dev.ps1 run
.\scripts\dev.ps1 build -Release

# Target-specific build; can be built while the old default executable is open:
.\scripts\dev.ps1 build -Release -Target x86_64-pc-windows-msvc

# Open the updated release executable:
.\target\x86_64-pc-windows-msvc\release\audio-matrix.exe

# GUI-only two-output preview (no audio starts):
.\scripts\dev.ps1 preview

# GUI-only view scrolled to the visible equalizers:
.\target\x86_64-pc-windows-msvc\release\audio-matrix.exe --preview-eq

# Explicit hardware regression: requires Realtek analog + connected LG ULTRAGEAR;
# opens muted playback streams only:
.\scripts\dev.ps1 test-hardware

# Native tray + endpoint watcher checks (no audio or registry writes):
.\scripts\dev.ps1 test-native

# Build/check the isolated kernel feasibility probe (does not install anything):
.\scripts\build-driver.ps1
.\scripts\build-driver.ps1 -Check

# Enumerate actual active Windows endpoints without starting routing:
.\scripts\dev.ps1 devices

# Full project checks (requires dependencies):
.\scripts\dev.ps1 check
.\scripts\dev.ps1 test
.\scripts\dev.ps1 lint

# DSP tests need only rustc, with no downloads:
.\scripts\test-core.ps1 -Optimize
```

If Cargo is configured for offline use on your development machine, set `$env:CARGO_NET_OFFLINE = 'false'` in a terminal with network access before the first build. Dependencies are now downloaded in this workspace, so subsequent builds can also run offline.

## Using the prototype once built

1. Select an input. For a virtual cable, configure your application to play to the cable's playback endpoint and select the cable's capture endpoint in **Capture device / virtual cable**. Loopback captures applications already playing to the selected output and does not silence the original playback.
2. Enable the outputs you want. Every enabled device has a card on the same dark soundboard page. Every device starts at -12 dB; surround outputs beyond matching input channels start disconnected. Output switches remain available while running.
3. Connect channels in each card's routing grid. Configure fine coefficients under the channel's visible **Source mix coefficients** controls. For stereo-to-mono, use 0.5 per source to preserve headroom for correlated signals.
4. Adjust device/channel faders, mutes, polarity, EQ and routing live. **Auto-align outputs** is enabled by default: it measures Windows playback positions and adds delay to faster streams. Choose **Main clock** on one enabled output to use its measured playback clock as the drift reference; this no longer adds a second audio buffer. Unchecking it uses the input clock. Removing or losing the main falls back to input timing. Input selection, device refresh, preset loading and buffer settings still require stopping.
5. Start routing and watch the output meters and buffer counters. A quiet 440 Hz test tone is available for a hardware smoke test. Stop immediately if the selected physical setup feeds a microphone back through speakers.
6. Allow a few seconds for clock measurements to settle. Each card shows automatic compensation and the estimated Windows offset. Use **Re-align** to refresh estimates without stopping. For any remaining audible echo, increase **Manual delay** on the faster-sounding device (0–500 ms, editable live). Manual trim is added to automatic compensation, not canceled by it.
7. Use **Global volume** and **Mute all** in the header to control all router outputs live, without changing their saved gains, routing, or delays. Global volume ranges from -60 to 0 dB and is smoothed over about 10 ms, before metering/clamping. It does not change Windows endpoint volume.
8. Scroll to the always-visible device/channel EQs. Drag knobs up/right to increase, hold Shift for fine adjustments, or focus a knob and use arrow keys. Double-click or Home resets that parameter. Frequency and Q use logarithmic scales. Presets include Flat, Bass boost, Bass cut, Voice clarity, Warm, and Bright: these are tonal starting points, not measured speaker correction profiles. Applying one changes only that EQ's bands, not gains, routing, or delay. For a custom shape, enter a name and click **Save EQ**; the same name updates an existing custom preset. The library supports up to 64 custom presets with 1–16 bands each.
9. Use footer **Open…**, **Save**, or **Save as…** for native Windows JSON file selection and saving. The active filename and modified/saved state replace the old always-visible path box; **Path** still provides manual entry and loading. Open requires stopped routing, while saving is available live. **Save EQ** alone stores a shape in memory; footer Save persists the routing configuration, global controls, EQ library, device names/order and stereo-link flags. Old presets preserve their gains, EQ, routes and calibration and default the added UI metadata. Presets never start audio automatically; new files require this updated app.

## Console UI workflow

- Resize the narrower sidebar by dragging its divider. Device names truncate instead of forcing wider panels; tooltips retain the complete Windows name. Three stereo cards fit across the default 1280-pixel window. EQs remain visible in the same vertically scrollable board, not drawers.
- Right-click a card heading to set a friendly name; use its `<`/`>` controls to reorder enabled devices and `x` to disable that output. Names and ordering persist with the routing preset.
- Faders have dB markings and a unity marker. Output meters use immediate attack, visual decay, and a 1.5-second peak hold. The header **OUTPUT PEAK** represents the loudest router output, not a sum or a separate master bus. Meter animation is UI-only; it does not modify audio or timing. Amber indicates limiting/headroom warnings, while red indicates clipping/faults.
- **Stereo link** gangs gain, mute and EQ of the first two channels. Existing gain offsets are retained; routes, polarity, manual delay and limiter configuration remain independent. Larger channel layouts are not automatically grouped.
- EQ graphs show a logarithmic frequency grid and draggable band points: horizontal movement changes frequency, vertical movement changes gain, and Shift gives fine control. Existing knobs remain available. **Copy/Paste** transfers EQ shapes across panels; **A/B** holds two temporary comparisons, with **A → B** to duplicate A. Comparison slots and the clipboard are session-only; the currently applied shape is what gets saved.
- **Undo/Redo** records up to 64 configuration changes and groups a pointer drag into one edit. Audio updates from restored configurations are sent live without restarting the source. Starting/stopping routing, refreshing devices, or successfully loading a preset clears the edit history so stale hardware layouts cannot be restored. It is not a persistent project history.
- Native file dialogs run on a helper thread; cancellation leaves settings unchanged, and Save As prompts before overwriting. Existing calibration files are never automatically rewritten or migrated. Live spectrum analysis is optional under **App**.

The router rejects capturing a playback endpoint and rendering back into that same endpoint. EQ boosts and summed routes can exceed full scale: leave headroom and monitor limiter gain reduction. The linked limiter normally protects the output; an emergency [-1, 1] clamp remains, especially when protection is bypassed. Pre-limiter overload counts are separate from actual emergency-clipped samples.

## Daily-use controls and reliability

- **Warning area:** bypass, heavy-limiting and emergency-clipping messages share a fixed two-line, 38-point space. Text truncates rather than expanding cards, keeping the matrix/EQ below it stable.
- **Reconnection:** Windows endpoint notifications wake a dedicated COM owner thread, with a five-second fallback scan. Offline outputs retain their enabled state, names, gains, EQ, routes and delays. Same-ID/same-channel-layout devices resume without restarting peers/source; the selected clock falls back temporarily to input and returns when its output reconnects. Disable a pending output to cancel retries. Changed channel layouts require stopping/refreshing and explicit re-enabling; common channel settings and device calibration are retained. A reinstalled device with a new endpoint ID is a new output, not guessed by name.
- **Tray:** **App → Hide to tray** keeps this process/routing alive. Tray click restores the board; the menu offers mute, volume ±1 dB, start/stop and explicit exit. **Close window to tray** defaults off. Exit always stops workers/removes the icon. Tray failure leaves ordinary window-close behavior available.
- **Session restore:** preferences, unsaved board state and up to 32 scenes are checkpointed separately in `%LOCALAPPDATA%\AudioMatrix\session.json`, after edits and on clean exit. This never overwrites the open routing preset. Restore defaults on; **Start audio after restoring** and **Run at Windows sign-in** are independent opt-ins. Sign-in registration changes only the current user's `AudioMatrix` Run value when its checkbox is explicitly changed. A saved offline input waits for that exact endpoint; choosing another input cancels the pending restore. Unreadable checkpoints are preserved with automatic checkpoint saving disabled.
- **Solo:** temporary device/channel audition changes only configurations sent to workers, preserving saved mutes/gains. Multiple targets form a union; existing mutes remain respected. **Clear solo** restores the configured mix. Solo/selection are not saved or recalled with presets/scenes.
- **Keyboard:** click a device heading or channel **Select**, then use **M** for mute or **S** for solo. Letter shortcuts are suppressed while editing fields. **Ctrl+M** toggles global mute, **Ctrl+S** saves, **Ctrl+Shift+S** opens Save As, **Ctrl+Z** undoes, and **Ctrl+Y / Ctrl+Shift+Z** redoes.
- **Scenes:** store/update a named mix in the header menu, recall it live, or delete with `x`. Scenes live in the separate checkpoint, not the routing preset. Recall requires the same input/source format; it changes output switches/routes/gains/mutes/EQ and global controls, retaining current delay, clock/auto-alignment, limiter settings, aliases and order. Gain/routes/EQ transitions are smoothed; device add/remove is not a sample-accurate cross-device transition.
- **EQ:** each band's **On** bypass retains settings. Choose Bell, LS (low shelf), HS (high shelf), HP (high pass) or LP (low pass); gain is unused/disabled for pass filters. DSP and graph share [RBJ cookbook coefficients](https://webaudio.github.io/Audio-EQ-Cookbook/audio-eq-cookbook.html). Old bands default to enabled Bell filters.
- **Spectrum:** **App → Live output spectrum** adds optional post-DSP/post-limiter device/channel FFT curves behind EQ responses. Blue uses its own −90..0 dBFS scale, not the EQ's ±24-dB response scale. A reusable 1024-point Hann FFT updates at most 20 Hz into 64 logarithmic bins; low-frequency resolution is limited. Device display uses maximum per-channel magnitude to avoid cancelling opposite-polarity channels. Analysis defaults off, uses extra CPU, and never modifies audio.

## Limiter and preserving audio quality

The least destructive strategy is to leave enough gain/EQ/mix headroom that limiting is needed only for occasional peaks. There is no lossless way to retain peaks above full scale at the same loudness: gain reduction changes dynamics, and already-clipped source audio cannot be reconstructed by a limiter.

Each output has an enabled-by-default, fully channel-linked **sample-peak** limiter after routing/EQ/device/channel/global gain and before metering/rendering. Defaults are a -1 dBFS ceiling, 5 ms look-ahead, and 120 ms release, with no makeup gain or automatic loudness boost. The ceiling (-12 to 0 dBFS), release (20–1000 ms), and bypass are editable live and saved per output. Old presets receive these defaults without changing their gains, EQ, routes, or manual delays. Reset DSP retains the limiter settings and timing calibration.

The limiter projects future peak constraints into a bounded monotonic envelope queue, ramps gain down before overloads arrive, and releases toward unity smoothly. A single gain envelope scales every channel of that device, preserving its channel ratios/polarity. Different devices have independent limiters, so a hot device can attenuate more than a quiet one. In default sample-peak mode, audio that never exceeds the ceiling is bit-identical through this stage after its fixed delay; configuration changes do not clear its delay history. Floating-point processing avoids an additional PCM quantization/encoding step, but the full router still applies EQ, gain and its existing interpolating resampler.

Every output retains the same look-ahead delay even when the limiter is bypassed (240 frames at 48 kHz, rounded to whole frames at other rates). Source-to-presentation mappings explicitly include this processing delay. Relative alignment settings remain valid, while end-to-end latency increases by approximately 5 ms. Pending final samples are discarded when a renderer stops, rather than replayed in a later session.

The UI shows gain reduction, pre-limiter input level, actual output peaks, protected-frame counts, and emergency clipping. A reduction above 3 dB triggers a headroom reminder; if it persists, lower device/global gain, reduce EQ boosts, or use smaller source-mix coefficients. The 3 dB reminder is a practical starting point, not a universal audible-distortion boundary. No audible music/perception test has yet established subjective transparency on this hardware.

Default protection remains sample-peak limiting. **4x inter-sample protection** is an opt-in conservative mode: a 32-tap windowed-sinc detector bounds all contributing samples at four interpolation phases and projects constraints into the same linked look-ahead envelope. It neither filters the emitted signal nor adds audio latency. Triangle bounds can attenuate more than sample-peak mode; genuinely quiet audio remains exact through this stage after its fixed delay. The **4x TP est.** uses that finite FIR and is not certified BS.1770 metering. It does not guarantee every continuous-time peak, DAC behavior, or subsequent Windows conversion. Leave headroom and validate listening quality. See [ITU-R BS.1770](https://www.itu.int/rec/R-REC-BS.1770) for standardized measurement.

## Architecture

```text
Windows apps -> virtual cable / future Matrix Input driver
                               |
                      WASAPI capture worker
                               |
                 one bounded shared source timeline
                      /                    \
              output worker A        output worker B
              independent cursor     independent cursor
              delay + resampling     delay + resampling
              channel matrix         channel matrix
              channel EQ             channel EQ
              device EQ              device EQ
              channel/device gain    channel/device gain
              global volume/mute     global volume/mute
              linked limiter         linked limiter
              meter + clamp          meter + clamp
              WASAPI render          WASAPI render
                    |                      |
              physical device A      physical device B
```

- `crates/matrix-core`: dependency-free routing, biquad EQ, smoothed gains/matrix coefficients, clipping accounting, a bounded two-second shared audio timeline, independent playback readers, and clock-alignment math. EQ uses peaking filters; frequency, gain and Q are editable.
- `src/audio`: WASAPI backend. COM audio objects are created and used on their owning worker thread. Capture retains the source's native channel layout and format; outputs request float audio at the source rate with the device's channel mask. Windows shared-mode conversion handles nominal sample-rate differences.
- `src/app.rs`: dark egui soundboard with all enabled outputs, inline device matrices, faders/meters and mutually exclusive main-clock selection.
- `crates/matrix-bridge`: allocation-free versioned stereo/48 kHz PCM packet validation/decoding for the future driver transport.
- `driver/matrix-input`: isolated Rust/WDK feasibility probe; no audio endpoint or transport implementation yet.
- `src/preset.rs`: versioned JSON, pre-application validation and replacement through a temporary file.

Device volume adjusts this router's stream, not Windows' global endpoint volume. Output channels correspond to the channels exposed by the device's Windows shared-mode configuration. Configure the device for 5.1/7.1 in Windows first if you need those channels. Channel labels use the endpoint's speaker mask where available.

Each renderer uses adaptive 32-tap Blackman-windowed sinc interpolation (256 interpolated phases, cutoff 0.97 of Nyquist), designed for these small drift ratios rather than arbitrary sample-rate conversion. Symmetric reads use retained history and preserve the source-frame mapping without adding a separate processing delay; Windows still handles nominal device-rate conversion. Input-referenced correction remains bounded to ±2000 ppm; main-clock followers combine the reference ratio with occupancy trim. This is not a hard real-time engine, and buffer target is not measured end-to-end latency. EQ changes crossfade stable filter banks over 20 ms and coalesce rapid edits; gain/mute/routes retain roughly 10-ms smoothing. Failed outputs retire independently and retry with a three-second backoff while enabled. An input failure still stops routing and requires manual restart; the app never silently changes inputs.

All outputs now read source audio directly from the same timeline; the main does not forward audio to followers. Main-clock selection changes only the resampling reference, without resetting source history or adding an extra buffering stage. Each renderer retains private routing/EQ/gain state.

Automatic alignment correlates [IAudioClock playback position and QPC timestamps](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getposition) with the source frames actually submitted to each stream, including initial silence and resampling. Clock frequency units are normalized. Invalid/startup/crossfade mappings and stale observations are excluded; estimates are smoothed, and delay changes have a 0.5 ms deadband and a 10 ms crossfade. Automatic compensation is bounded to 500 ms per output, targeting the slowest measured stream; manual calibration is additive. Unsupported clocks fall back to manual delay and input/occupancy drift correction. Removing the last peer clears unnecessary automatic delay.

These are **Windows stream measurements, not acoustic measurements**. Internal monitor/speaker DSP, Bluetooth transport, acoustic distance and microphone latency can remain unobserved. Auto-align therefore cannot promise sample-accurate speaker synchronization. Use manual trim for those residual offsets. A future opt-in microphone calibration could estimate acoustic delay; this app does not record a calibration microphone or emit calibration sounds automatically.

## Next milestones

1. Extend listening/soak tests with actual application playback, presets, endpoint unplug/replug, and 5.1/7.1 mapping on suitable hardware. Live switching and clock changes have short muted hardware regression coverage; long-term audible quality and latency still need measurement.
2. Continue **Matrix Input**: validate PortCls/WaveRT Rust bindings, implement a stereo/48 kHz render endpoint and producer clock, then secure bounded kernel-to-user PCM transport and the engine reader. Establish a Windows test VM with kernel debugging before loading experimental drivers. The build-only WDM probe and packet contract are foundations, not this endpoint. See [driver milestones](driver/README.md). Microsoft's [Rust driver tooling](https://github.com/microsoft/windows-drivers-rs) remains experimental.
3. Validate perceptual transparency and CPU cost of the sinc interpolator, EQ transitions and optional conservative 4x protection. Certified true-peak metering/limiting and opt-in microphone-assisted acoustic calibration remain future work.
4. Add driver packaging/signing, a separately hosted background routing service and installer. Tray operation keeps this desktop process alive; it is not a Windows service. ASIO/exclusive mode and multiple sources remain later expansions.

Driver references: Microsoft's [SysVAD sample](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad), [WaveRT miniport development](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/developing-a-wavert-miniport-driver) and [signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-). Backend: [wasapi-rs](https://github.com/HEnquist/wasapi-rs).
