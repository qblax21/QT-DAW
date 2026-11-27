# QT DAW

A custom digital audio workstation for Windows (Mac later), built for recording live band
sessions (18+ inputs from a Behringer X32), recording one part at a time over a click,
and mixing/mastering, with a Claude MCP server so Claude can do first-pass mixes.

## Roadmap

| Phase | What you get |
|---|---|
| **0. Hardware probe** (done) | Project setup; QT DAW sees your interface over ASIO and meters every input live |
| **1. Recorder** (done) | App window; record any number of armed tracks to disk, playback, click + count-in, tempo, input monitoring, volume/pan/mute/solo, save/open sessions |
| **2. Mixer** (done) | Mixer view (X), aux buses, pre/post-fader sends, track output routing, console-style faders, meters with peak hold and clip lights |
| **3. Built-in effects** (done) | Channel EQ, compressor, noise gate, de-esser, limiter, delay, reverb on tracks, buses and the master; chain presets (vocal, drums, reverb bus, master...) |
| **4. Claude MCP** (done) | Claude can analyze the mix (loudness, peaks, tone, masking) and set faders, pans, sends, routing and effects |
| 5. Editing | Cut, move, fades, crossfades (non-destructive), undo |
| 6. X32 control surface | X32 faders/mutes drive QT DAW via its DAW Remote (Mackie/HUI) mode |
| 7. MIDI + instruments | Drum sampler with our own kits, simple synths |
| 8. Later | Take comping, punch in/out, pitch correction, VST3 hosting, Mac build |

## Layout

- `crates/qtdaw-app` — the desktop app (`qtdaw.exe`), built with egui
- `crates/qtdaw-engine` — audio engine: devices, transport, click, recording, sessions
- `crates/qtdaw-mcp` — `qtdaw-mcp.exe`, the MCP server that connects Claude to the running app
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
  **X** mixer / tracks · **Delete** remove selected clip · **Ctrl+S** save · **Ctrl+T** new track.
- **Mixer (X):** one strip per track, then the buses, then the master. On a strip, top to
  bottom: input, sends, output, pan, volume, fader + meter, R/M/S, name.
  - **Buses** (`+ Bus`, or "New Bus" in any send/output menu) are stereo aux channels.
    Set a track's **output** to a bus to group tracks (all drums → "Drums" bus, one fader).
    Use a **send** for effects like reverb: the track still plays normally, and a copy goes to
    the bus at the send knob's level, and a Reverb on the bus does the rest.
  - Sends are post-fader (follow the track's fader) unless you tick **Pre-fader** in the
    send's menu (yellow knob). Right-click a strip's name to delete it.
  - Faders: drag (hold **Shift** for fine moves), double-click for 0 dB, or type a value in
    the box above. The meter's red light at the top means the signal went over 0 dBFS;
    click it to reset.
  - Solo a bus to hear it and everything feeding it. Solo a track and its buses stay on, so
    you still hear its reverb.
- **Audio FX (effects):** the slots near the top of each mixer strip. Click **Audio FX…** to add
  an effect, or pick **Chain Presets** for a ready-made chain (Lead Vocal, Kick, Reverb Bus,
  Master...). Up to 8 effects per channel; they run top to bottom.
  - Click an effect's slot to open its window; the dot on the left turns it on/off (bypass)
    so you can compare. Right-click a slot to move it up/down or remove it.
  - Knobs: drag up/down (Shift = fine), double-click for the default. On the EQ you can also
    drag the colored dots on the curve, and scroll over a Mid dot to make it wider/narrower.
  - Compressor, gate, de-esser and limiter show **Reduction**: how many dB they are turning
    the sound down right now.
  - Reverb and delay sound best on a bus (Mix 100%) that tracks send to, so several tracks
    share one room. The Reverb Bus / Delay Bus presets set that up.
  - Track effects come before the fader; master effects come after the master fader, so a
    limiter's ceiling always holds.
- Stop returns to where you started playing; Stop again goes to the beginning.
- **Ctrl+mouse wheel** zooms the timeline, **Shift+wheel** scrolls it. Click the ruler or
  an empty lane to move the playhead. Right-click a track header to delete it.
- Sessions live in `Documents\QT DAW\<name>\` (`session.qtdaw` + `audio\*.wav`, 24-bit).
  Copy the folder to move a session between computers. Takes are saved the moment you stop.
- Recordings are shifted earlier by the measured driver latency so they line up with what
  you heard. If they still land early/late, adjust **Recording offset** in Audio Settings.

## Mixing with Claude

QT DAW comes with `qtdaw-mcp.exe`, an MCP server: a small program Claude starts so it can
work in the session that's open in QT DAW. Claude can:

- **Read** the session (tracks, buses, routing, effects and their settings).
- **Analyze** the mix: it plays the song offline (with all effects, in a second or two) and
  measures loudness (LUFS, like streaming services), loudness range, true peak, clipping,
  tone per frequency region, stereo correlation, and **masking**: tracks that are both
  strong at the same frequencies at the same time (e.g. bass and keys fighting at 250 Hz).
- **Mix**: set volume/pan/mute/solo, route tracks to buses, add buses and sends, add, change,
  bypass or remove effects, add chain presets, play/stop, and save (only when you say so).

Connect it once. First copy the connector to its own folder, so Claude running it never
blocks a rebuild (`tools\build.ps1` keeps that copy up to date afterwards):

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\QT DAW"; Copy-Item target\debug\qtdaw-mcp.exe "$env:LOCALAPPDATA\QT DAW\"
```

Then:

- **Claude Desktop** (the chat app): Settings → Developer → Edit Config, and put this in
  `claude_desktop_config.json`, then restart Claude Desktop:

  ```json
  { "mcpServers": { "qtdaw": { "command": "C:\\Users\\<you>\\AppData\\Local\\QT DAW\\qtdaw-mcp.exe" } } }
  ```

- **Claude Code**: run this once in a terminal:

  ```powershell
  claude mcp add --scope user qtdaw -- "$env:LOCALAPPDATA\QT DAW\qtdaw-mcp.exe"
  ```

Then open your session in QT DAW and ask Claude something like *"Do a first-pass mix of
my song in QT DAW"* or *"Why does my vocal sound buried?"*.

- QT DAW must be running (it can be minimized). The status bar shows each change Claude
  makes ("Claude: Bass: volume -3.0 dB").
- Before Claude's first change in a session, QT DAW saves a copy of the session as it was:
  `session.before-claude.qtdaw` in the session folder. To go back, close QT DAW, delete
  `session.qtdaw` and rename that file to `session.qtdaw`.
- Claude can't record, delete tracks or touch your audio files.
- The connection only accepts programs on this computer (127.0.0.1, port 47390; set the
  `QTDAW_PORT` environment variable for both programs to change it).

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
- Buses feed the master only (no bus → bus routing yet); up to 32 buses per session.
- Tracks are mono, so effects on a track are mono too (reverb/delay are stereo on a bus).
- No delay compensation yet: the limiter delays its channel by 1.5 ms (harmless on the master).
- Claude's changes can't be undone yet (Phase 5); see the backup file under "Mixing with Claude".
- Claude's per-track numbers include the track's effects, fader and bus fader, but not bus or
  master effects. Analysis ignores solo (so it always hears the whole mix).
