# macOS capture notes

Status: analysis only (2026-10-02). Nothing is built or tested on a Mac. API availability and crate maturity are unverified.

## Summary

Native macOS system-audio capture is possible. Only the capture half needs to change. The WebSocket transport and output code (`src/bin/mimikiku-capture.rs`, `src/lib/net.rs`) are OS-independent and can be reused.

## Why the Linux approach doesn't carry over

On Linux, `cpal` picks a PulseAudio or PipeWire "monitor" input, which carries speaker output. macOS has no monitor input. The CoreAudio backend only exposes real inputs such as the mic.

## Options

| Option | macOS | Notes |
|---|---|---|
| ScreenCaptureKit (`SCStream`, audio enabled) | 13+ | System or per-app audio, no driver. Needs Screen Recording permission. |
| Core Audio process taps (`CATapDescription`) | 14.2+ | Audio only, can target specific apps. Needs Audio Recording permission. |
| BlackHole or Loopback (virtual device) | any | The existing `cpal` code works. The user installs a driver and sets up a multi-output device. |

**Recommendation:** process taps, if macOS 14.2+ is acceptable. The permission prompt is milder, and per-app capture suits audiobooks. Fall back to ScreenCaptureKit for macOS 13.

## Rust crates

- `screencapturekit`: bindings for ScreenCaptureKit. The more mature option.
- `objc2-*`: raw Core Audio bindings. Check whether the process-tap bindings are complete before relying on them.
- `cpal`: newer versions may support loopback on some platforms. Unverified for macOS.

## Work involved

1. Capture audio, usually 48 kHz stereo f32.
2. Downmix and resample to 16 kHz mono f32, in 2048-sample chunks. `rubato` is a candidate.
3. Send over the existing base64 protocol. No server change.
4. Handle permissions. A signed `.app` bundle is more reliable than an unsigned CLI.

## Recommendation for structure

Put macOS capture in a separate crate, for example `mimikiku-capture-mac`. The two capture paths share little code. Copy the transport code or move it into a small shared crate.

## Open questions

- Minimum macOS version.
- System-wide or per-app capture.
- Whether the Mac reaches the GPU server over the network (the server stays on Linux).
