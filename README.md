# QT DAW

A custom digital audio workstation for Windows (Mac later), built for recording live band
sessions (18+ inputs from a Behringer X32), recording one part at a time over a click,
and mixing/mastering, with a Claude MCP server so Claude can do first-pass mixes.

## Roadmap

| Phase | What you get |
|---|---|
| **0. Hardware probe** (done) | Project setup; QT DAW sees your interface over ASIO and meters every input live |
| 1. Recorder | Record 18+ tracks to disk, playback, transport, click + count-in, tempo, save/load sessions |
| 2. Mixer | Faders, pan, mute/solo, meters, buses, sends |
| 3. Built-in effects | EQ, compressor, gate, de-esser, reverb, delay, limiter, vocal chain presets |
| 4. Claude MCP | Analysis (loudness, spectrum, masking) + mix controls for Claude |
| 5. Editing | Cut, move, fades, crossfades (non-destructive) |
| 6. X32 control surface | X32 faders/mutes drive QT DAW via its DAW Remote (Mackie/HUI) mode |
| 7. MIDI + instruments | Drum sampler with our own kits, simple synths |
| 8. Later | Take comping, punch in/out, pitch correction, VST3 hosting, Mac build |

## Layout

- `crates/qtdaw-engine` — audio engine (devices, stream I/O, routing, meters)
- `crates/qtdaw-probe` — command-line hardware test tool
- `third_party/asio-sys` — patched copy of the ASIO bindings (see "QT DAW patch" in its `build.rs`)
- `tools/build.ps1` — build script (sets up the MinGW toolchain)

## Build

One-time: `winget install --id BrechtSanders.WinLibs.POSIX.MSVCRT.LLVM --exact --scope user`

```powershell
powershell -ExecutionPolicy Bypass -File tools\build.ps1
```

The first ASIO build downloads the Steinberg ASIO SDK automatically.

## Try it

```powershell
.\target\debug\qtdaw-probe.exe devices
```

```powershell
.\target\debug\qtdaw-probe.exe live --host asio --input "AudioBox" --buffer 128 --monitor 1
```

`--monitor 1` lets you hear input 1 in the headphones; `--monitor 1,2` is stereo. Without
it you only see meters. For the X32 the input name will be something like `"X-USB"`.
