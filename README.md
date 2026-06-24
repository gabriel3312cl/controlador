# remotedesk

Remote desktop LAN-only. One binary, two modes:

- **macOS** → server: captures the screen, encodes with VideoToolbox (HEVC), streams via TCP
- **Windows** → client: receives the stream, decodes with FFmpeg, renders fullscreen

No cloud, no relay, no auth. Direct IP:port over your local network. Mouse and keyboard input travels back from client to server.

## Download

Go to [Releases](https://github.com/gserra/controlador/releases) and grab the latest:

| Platform | File | Notes |
|---|---|---|
| macOS (Apple Silicon) | `remotedesk` | Standalone binary. May need `xattr -cr remotedesk` after download. |
| Windows (x64) | `remotedesk-windows.zip` | Unzip and run `remotedesk.exe`. No installer. |

## How to use

### macOS (server)

1. Open **System Preferences → Privacy & Security → Accessibility** and allow the terminal (or the binary) to control your computer. Without this, remote inputs won't work.
2. Run the binary (double-click or `./remotedesk` in Terminal).
3. A small window shows your local IP. The server is already listening — nothing else to do.

### Windows (client)

1. Unzip `remotedesk-windows.zip`.
2. Run `remotedesk.exe`.
3. Type the macOS server's IP (shown in the server window) and click **Conectar**.
4. The window expands to fullscreen with the remote desktop. Mouse and keyboard are forwarded automatically.

## How it works

```
macOS (server)                          Windows (client)
┌──────────────────────────┐            ┌──────────────────────┐
│ Capture (CGDisplay)      │            │                      │
│      ↓                   │   TCP      │   Decode (FFmpeg)    │
│ Encode (VideoToolbox)    │─── 7070 ──→│      ↓               │
│      ↓                   │            │   egui texture       │
│ Serve (tokio TCP)        │            │      ↓               │
│                          │←───────────│   Input hooks        │
│ Inject (CoreGraphics)    │  inputs    │   (mouse/keyboard)   │
└──────────────────────────┘            └──────────────────────┘
```

Resolution is auto-detected from the host display. Coordinates are normalized (0.0–1.0) so the client doesn't need to know the exact resolution.

## Build from source

### macOS

```bash
cargo build --release
# Binary at target/release/remotedesk
```

### Windows

You need [FFmpeg shared libraries](https://www.gyan.dev/ffmpeg/builds/) (the `full-shared` build). Extract them and add the `bin/` folder to your `PATH`, or copy the DLLs next to the binary.

```powershell
$env:FFMPEG_DIR = "C:\path\to\ffmpeg"
cargo build --release
# Binary at target\release\remotedesk.exe
```

Required DLLs: `avcodec-*.dll`, `avutil-*.dll`, `swscale-*.dll`, `swresample-*.dll`, `avformat-*.dll`

## Tech stack

- Rust 2021 edition
- egui / eframe 0.27 — UI
- tokio — async TCP
- VideoToolbox (FFI) — H.264/HEVC hardware encoding (macOS)
- CoreGraphics (FFI) — screen capture & input injection (macOS)
- ffmpeg-next — H.264/HEVC software decoding (Windows)
- Win32 hooks — input capture (Windows)

## License

MIT
