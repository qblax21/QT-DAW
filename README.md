# QT DAW

A custom digital audio workstation for Windows (Mac later), built for recording live band
sessions (18+ inputs from a Behringer X32), recording one part at a time over a click,
and mixing/mastering, with a Claude MCP server so Claude can do first-pass mixes.

## Roadmap

| Phase | What you get |
|---|---|
| **0. Hardware probe** (done) | Project setup; QT DAW sees your interface over ASIO and meters every input live |
| **1. Recorder** (done) | App window; record any number of armed tracks to disk, playback, click + count-in, tempo, input monitoring, volume/pan/mute/solo, save/open sessions |
| 2. Mixer | Mixer view, buses, sends, better meters |
| 3. Built-in effects | EQ, compressor, gate, de-esser, reverb, delay, limiter, vocal chain presets |
| 4. Claude MCP | Analysis (loudness, spectrum, masking) + mix controls for Claude |
| 5. Editing | Cut, move, fades, crossfades (non-destructive), undo |
| 6. X32 control surface | X32 faders/mutes drive QT DAW via its DAW Remote (Mackie/HUI) mode |
| 7. MIDI + instruments | Drum sampler with our own kits, simple synths |
| 8. Later | Take comping, punch in/out, pitch correction, VST3 hosting, Mac build |

## Layout

- `crates/qtdaw-app` — the desktop app (`qtdaw.exe`), built with egui
- `crates/qtdaw-engine` — audio engine: devices, transport, click, recording, sessions
- `crates/qtdaw-probe` — command-line hardware test tool
- `third_party/asio-sys` — patched copy of the ASIO bindings (see "QT DAW patch" in its `build.rs`)
- `tools/build.ps1` — build script (sets up the MinGW toolchain)

## Build and run

One-time: `winget install --id BrechtSanders.WinLibs.POSIX.MSVCRT.LLVM --exact --scope user`

```powershell
powershell -ExecutionPolicy Bypass -File tools\build.ps1
```

```powershell
.\target\debug\qtdaw.exe
```

The first ASIO build downloads the Steinberg ASIO SDK automatically.

## Using it

- **Settings → Audio Settings** picks the interface. ASIO = lowest delay (needs the
  interface's ASIO driver). Cheap class-compliant USB interfaces use WASAPI, which only
  runs at the sample rate set for the device in Windows Sound settings.
- **R** on a track arms it for recording *and* lets you hear its input.
- Keys: **Space** play/stop · **R** record · **Enter** go to start · **K** click on/off ·
  **Delete** remove selected clip · **Ctrl+S** save · **Ctrl+T** new track.
- Stop returns to where you started playing; Stop again goes to the beginning.
- **Ctrl+mouse wheel** zooms the timeline, **Shift+wheel** scrolls it. Click the ruler or
  an empty lane to move the playhead. Right-click a track header to delete it.
- Sessions live in `Documents\QT DAW\<name>\` (`session.qtdaw` + `audio\*.wav`, 24-bit).
  Copy the folder to move a session between computers. Takes are saved the moment you stop.
- Recordings are shifted earlier by the measured driver latency so they line up with what
  you heard. If they still land early/late, adjust **Recording offset** in Audio Settings.

## Hardware tests

```powershell
.\target\debug\qtdaw-probe.exe devices
```

```powershell
.\target\debug\qtdaw-probe.exe live --host asio --input "AudioBox" --buffer 128 --monitor 1
```

```powershell
.\target\debug\qtdaw-probe.exe record --host asio --input "AudioBox" --buffer 128 --inputs 1,2 --seconds 5
```

For the X32 the device name will be something like `"X-USB"`.

## Known limits (for now)

- Clips are held in RAM (about 11.5 MB per mono minute at 48 kHz).
- Only one interface at a time (two separate USB interfaces can't be combined yet).
- No undo, no clip moving yet (Phase 5).
