# Matrix Input driver groundwork

The desktop router works independently of this directory. The WDM probe builds
with Rust, MSVC, the Windows SDK/WDK 10.0.22621 and LLVM 17.0.6. Its PE headers
were checked: x64, Native subsystem, DriverEntry entry point, and ntoskrnl.exe
imports. It deliberately returns STATUS_NOT_SUPPORTED. It creates no devices
and cannot be used as an audio endpoint.

No driver was installed, no certificate was added, and no test-signing or boot
setting was changed. Do not install the probe on the development PC.

## Build

From the repository root:

```powershell
.\scripts\build-driver.ps1
.\scripts\build-driver.ps1 -Check
.\scripts\dev.ps1 test
```

The driver has a separate Cargo workspace, lockfile, abort-on-panic policy and
static CRT setting. Desktop builds do not require the WDK or LLVM. The build
script restores process environment variables and never installs or signs
anything. Cargo emits a DLL-named PE which is linked with /DRIVER,
/SUBSYSTEM:NATIVE and /ENTRY:DriverEntry; the script also copies it to
`driver/matrix-input/target/x86_64-pc-windows-msvc/release/matrix_input.sys`.

## PCM contract v1

`crates/matrix-bridge` is dependency-free, no_std and allocation-free. It
validates and decodes complete packets using explicit little-endian fields;
there are no pointers, Rust struct layouts or kernel addresses on the wire.
The initial endpoint format is stereo at 48 kHz, signed PCM16 or float32,
interleaved L/R, with 1–4800 frames per packet. Silent packets carry no payload.
Float decoding sanitizes NaN and infinity. This packet contract is implemented
and tested; the kernel-to-user transport carrying it is not implemented yet.

| Byte offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | Magic: MXPC |
| 4 | 2 | Protocol version: 1 |
| 6 | 2 | Header size: 32 |
| 8 | 4 | Sample rate: 48000 |
| 12 | 2 | Channels: 2 |
| 14 | 2 | Encoding: 1 = PCM16; 3 = float32 |
| 16 | 4 | Frame count |
| 20 | 8 | Packet sequence; wraps at u64::MAX |
| 28 | 4 | Flags: bit 0 discontinuity, bit 1 silent |
| 32 | variable | Exactly frame count × channels × sample bytes, or zero for silent |

## Next implementation gate

1. Generate and validate PortCls/WaveRT interface bindings from the installed
   WDK headers. The existing probe establishes general WDM feasibility, not
   the audio miniport ABI.
2. Implement a real render endpoint with topology and WaveRT miniports,
   initially stereo/48 kHz. Windows must enumerate it as Matrix Input and a
   WASAPI client must be able to open and play into it.
3. Implement the producer clock, bounded PCM ring and a secured service-only
   transport. Define ownership, overrun policy, disconnect behavior, sequence
   resets and cancellation before exposing IOCTLs/shared memory. Never stall
   Windows playback when the UI or engine is absent.
4. Add a Rust engine reader and handle transport discontinuities, then route
   actual application audio from Matrix Input to two physical outputs.
5. Validate lifecycle/power/cancellation in an isolated Windows VM with kernel
   debugging and Driver Verifier. Prepare INF/catalog and test signing only
   for that VM, with explicit approval before changing security/boot settings.
   Distribution signing and an installer follow after this passes.

The Rust driver tooling is experimental. Do not assume that successful kernel
linking proves the PortCls/WaveRT miniport is safe or complete.

References:

- [Microsoft Rust driver tooling](https://github.com/microsoft/windows-drivers-rs)
- [Developing a WaveRT miniport driver](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/developing-a-wavert-miniport-driver)
- [Microsoft SysVAD audio sample](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad)
- [Driver signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)
