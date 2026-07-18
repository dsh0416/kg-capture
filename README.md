# KG Capture

KG Capture reads WeSing (全民K歌) lyric timing data and renders the lyrics in an independent, DPI-aware iced window. It does not transfer captured screen images.

KG Capture 从全民 K 歌进程读取歌词时间轴和播放位置，并在独立的 iced 窗口中按当前 DPI 重新渲染。默认实现不传输截图或像素帧。

## Architecture

The workspace is split by process architecture:

- `kg-capture.exe`: x64 iced controller and semantic lyric renderer.
- `kg-capture-injector.exe`: x86 helper that launches WeSing suspended, loads the hook DLL, and resumes the child process.
- `kg_capture_hook.dll`: x86 DLL using Retour runtime hooks around WeSing's lyric render-model update methods.
- `kg-capture-protocol`: versioned, pointer-free Serde messages transported with Servo's `ipc-channel`.
- `kg-capture-fixture`: x86 semantic test source used by `cargo xtask smoke`.

The hook locates `CLyricRenderWnd` and `CLyricRenderWndForLiveShow` from RTTI in `KSongsUI.dll`, verifies the known update-function prologue, and only then installs detours. Unsupported DLL builds fail closed instead of using unchecked fixed addresses.

IPC carries two kinds of semantic updates:

- `LyricTimeline`: line text, per-line timing, and per-word timing; sent when the lyric model changes.
- `PlaybackPosition`: current time, active line, and line progress; sent while playback advances.

The iced process performs all text layout and highlighting, so rendering follows its own logical-pixel scale rather than WeSing's GDI/GDI+ DPI behavior.

The host uses separate windows: `KG Capture` contains connection, diagnostic, and lyric appearance controls, while `KG Lyrics` contains only the lyric presentation intended for OBS capture. The control window can adjust the lyric background color, text color, playback highlight color, active-line font size, candidate-line font size, and left/center/right alignment in real time. Its font list is populated at startup from the installed Windows font families through DirectWrite and uses localized display names from the preferred Windows UI languages. The lyric window can be closed independently and reopened from the control window.

## Build

Requirements:

- Stable Rust with `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` targets.
- Visual Studio Build Tools with the corresponding x64 and x86 MSVC tools.

Build and stage the mixed-architecture release:

```powershell
cargo xtask build
```

The command produces and verifies:

```text
dist/kg-capture.exe            x64
dist/kg-capture-injector.exe   x86
dist/kg_capture_hook.dll       x86
```

Build, stage, and run the application:

```powershell
cargo xtask run
```

`xtask run` uses a unique directory under `target/kg-capture-runs/` for each launch. A previously running KG Capture window therefore does not lock or prevent staging the next build. `cargo xtask build` remains the command for explicitly publishing the latest artifacts to the fixed `dist/` directory.

`KG_CAPTURE_INJECTOR_PATH` and `KG_CAPTURE_HOOK_PATH` can override component locations.

Run the automated x86 suspended-launch, DLL injection, cross-architecture IPC, timeline, and playback-position test:

```powershell
cargo xtask smoke
```

Run formatting checks, unit tests, strict architecture-specific Clippy checks, release builds, and the smoke test:

```powershell
cargo xtask test
```

## Usage

1. Start `kg-capture.exe`.
2. Completely exit any running WeSing instance (including its background process), then browse to the x86 `WeSing.exe`. KG Capture will refuse to launch another copy while `WeSing.exe` is still running because WeSing's single-instance check would bypass the new hooked process.
3. Select **启动 WeSing**. KG Capture launches it as a suspended child, injects the x86 hook, then resumes it with WeSing's installation directory as its working directory.
4. KG Capture automatically starts lyric synchronization after the hook connects. Open a song or another WeSing view that loads lyrics; the first semantic timeline may arrive when playback starts or the lyric view next updates.
5. Capture the iced KG Lyrics window in OBS if desired.

Launching WeSing as a child avoids requesting access to an unrelated existing process. Attaching to an existing process remains an injector-only diagnostic mode.

## Compatibility and diagnostics

The current semantic reader is validated against WeSing/`KSongsUI.dll` version `2.21.176.1220`. A different binary may have a different internal lyric structure. The hook checks RTTI and function bytes and reports an unsupported-version error instead of installing a guessed detour.

Each launch creates a session directory under:

```text
%LOCALAPPDATA%\kg-capture\logs\<timestamp>-<host-pid>-<session>
```

The directory is intentionally kept out of the UI. It contains:

- `host.log`: session lifecycle, IPC handshake, warnings, and errors.
- `injector.log`: process launch, DLL injection, and `kg_capture_start` status.
- `hook.log`: `KSongsUI.dll` loading, hook lifecycle, timeline changes, warnings, and errors.

Logs default to `INFO` and above. Set `RUST_LOG=kg_capture=debug` before launching the host to include per-event records, callback counts, PE/RTTI details, timeline pointers, and accepted/rejected lyric diagnostics. When reporting a failure, include all three files; enable `DEBUG` first when investigating whether a WeSing view calls the known lyric update methods.

The hook never sends target-process pointers over IPC; all UTF-16 strings and timing values are copied into owned Rust values first, with line, word, string-length, and readable-memory bounds.
