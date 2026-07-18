# KG Capture

KG Capture reads WeSing (全民K歌) lyric timing data and renders the lyrics in an independent, DPI-aware iced window. It does not transfer captured screen images.

KG Capture 从全民 K 歌进程读取歌词时间轴和播放位置，并在独立、支持 DPI 感知的 iced 窗口中重新渲染歌词。它不会传输屏幕截图。

## Architecture / 架构

The workspace is split by process architecture:

工作区按进程架构拆分：

- `kg-capture.exe`: x64 iced controller and semantic lyric renderer.
  `kg-capture.exe`：x64 iced 控制器和语义化歌词渲染器。
- `kg-capture-injector.exe`: x86 helper that launches WeSing suspended, loads the hook DLL, and resumes the child process.
  `kg-capture-injector.exe`：x86 辅助程序，以挂起状态启动全民 K 歌、加载钩子 DLL，然后恢复子进程。
- `kg_capture_hook.dll`: x86 DLL using Retour runtime hooks around WeSing's lyric render-model update methods.
  `kg_capture_hook.dll`：x86 DLL，使用 Retour 在运行时挂钩全民 K 歌的歌词渲染模型更新方法。
- `kg-capture-protocol`: versioned, pointer-free Serde messages transported with Servo's `ipc-channel`.
  `kg-capture-protocol`：通过 Servo 的 `ipc-channel` 传输带版本且不含指针的 Serde 消息。
- `kg-capture-fixture`: x86 semantic test source used by `cargo xtask smoke`.
  `kg-capture-fixture`：供 `cargo xtask smoke` 使用的 x86 语义测试数据源。

The hook locates `CLyricRenderWnd` and `CLyricRenderWndForLiveShow` from RTTI in `KSongsUI.dll`, verifies the known update-function prologue, and only then installs detours. Unsupported DLL builds fail closed instead of using unchecked fixed addresses.

钩子通过 `KSongsUI.dll` 中的 RTTI 定位 `CLyricRenderWnd` 和 `CLyricRenderWndForLiveShow`，验证已知更新函数的序言字节后才安装跳转钩子。不受支持的 DLL 版本会安全退出，而不会使用未经检查的固定地址。

IPC carries two kinds of semantic updates:

IPC 传输两类语义化更新：

- `LyricTimeline`: line text, per-line timing, and per-word timing; sent when the lyric model changes.
  `LyricTimeline`：包含歌词行文本、逐行时间轴和逐字时间轴；在歌词模型发生变化时发送。
- `PlaybackPosition`: current time, active line, and line progress; sent while playback advances.
  `PlaybackPosition`：包含当前时间、活动歌词行和行内进度；在播放推进时发送。

The iced process performs all text layout and highlighting, so rendering follows its own logical-pixel scale rather than WeSing's GDI/GDI+ DPI behavior.

iced 进程负责全部文本布局和高亮，因此渲染遵循自身的逻辑像素缩放，而不受全民 K 歌 GDI/GDI+ DPI 行为的影响。

The host uses separate windows: `KG Capture` contains connection, diagnostic, and lyric appearance controls, while `KG Lyrics` contains only the lyric presentation intended for OBS capture. The control window can adjust the lyric background color, text color, playback highlight color, active-line font size, candidate-line font size, and left/center/right alignment in real time. Its font list is populated at startup from the installed Windows font families through DirectWrite and uses localized display names from the preferred Windows UI languages. The lyric window can be closed independently and reopened from the control window.

宿主程序使用两个独立窗口：`KG Capture` 提供连接、诊断和歌词外观控制，`KG Lyrics` 仅显示供 OBS 采集的歌词。控制窗口可实时调整歌词背景色、文字颜色、播放高亮颜色、活动行字号、候选行字号以及左对齐、居中或右对齐。程序启动时通过 DirectWrite 读取已安装的 Windows 字体系列，并按照首选 Windows UI 语言显示本地化字体名称。歌词窗口可以独立关闭，并可从控制窗口重新打开。

## Build / 构建

Requirements:

环境要求：

- Stable Rust with `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` targets.
  安装了 `x86_64-pc-windows-msvc` 和 `i686-pc-windows-msvc` 目标的稳定版 Rust。
- Visual Studio Build Tools with the corresponding x64 and x86 MSVC tools.
  安装了相应 x64 和 x86 MSVC 工具的 Visual Studio Build Tools。

Build and stage the mixed-architecture release:

构建并暂存混合架构发布产物：

```powershell
cargo xtask build
```

The command produces and verifies:

该命令会生成并验证：

```text
dist/kg-capture.exe            x64
dist/kg-capture-injector.exe   x86
dist/kg_capture_hook.dll       x86
```

Build, stage, and run the application:

构建、暂存并运行应用程序：

```powershell
cargo xtask run
```

`xtask run` uses a unique directory under `target/kg-capture-runs/` for each launch. A previously running KG Capture window therefore does not lock or prevent staging the next build. `cargo xtask build` remains the command for explicitly publishing the latest artifacts to the fixed `dist/` directory.

`xtask run` 每次启动时都会使用 `target/kg-capture-runs/` 下的独立目录。因此，先前运行的 KG Capture 窗口不会锁定或阻止下一次构建的暂存。若要将最新产物明确发布到固定的 `dist/` 目录，仍应使用 `cargo xtask build`。

`KG_CAPTURE_INJECTOR_PATH` and `KG_CAPTURE_HOOK_PATH` can override component locations.

可通过 `KG_CAPTURE_INJECTOR_PATH` 和 `KG_CAPTURE_HOOK_PATH` 覆盖组件路径。

Run the automated x86 suspended-launch, DLL injection, cross-architecture IPC, timeline, and playback-position test:

运行自动化测试，验证 x86 挂起启动、DLL 注入、跨架构 IPC、歌词时间轴和播放位置：

```powershell
cargo xtask smoke
```

Run formatting checks, unit tests, strict architecture-specific Clippy checks, release builds, and the smoke test:

运行格式检查、单元测试、针对各架构的严格 Clippy 检查、发布构建和冒烟测试：

```powershell
cargo xtask test
```

## Usage / 使用方法

1. Start `kg-capture.exe`.
   启动 `kg-capture.exe`。
2. Completely exit any running WeSing instance (including its background process), then browse to the x86 `WeSing.exe`. KG Capture will refuse to launch another copy while `WeSing.exe` is still running because WeSing's single-instance check would bypass the new hooked process.
   完全退出所有正在运行的全民 K 歌实例（包括后台进程），然后选择 x86 版本的 `WeSing.exe`。如果 `WeSing.exe` 仍在运行，KG Capture 会拒绝启动新实例，因为全民 K 歌的单实例检查会绕过新创建的已挂钩进程。
3. Select **启动 WeSing**. KG Capture launches it as a suspended child, injects the x86 hook, then resumes it with WeSing's installation directory as its working directory.
   点击 **启动 WeSing**。KG Capture 会以挂起的子进程启动全民 K 歌，注入 x86 钩子，然后以全民 K 歌安装目录作为工作目录恢复进程。
4. KG Capture automatically starts lyric synchronization after the hook connects. Open a song or another WeSing view that loads lyrics; the first semantic timeline may arrive when playback starts or the lyric view next updates.
   钩子连接后，KG Capture 会自动开始歌词同步。打开歌曲或全民 K 歌中其他会加载歌词的页面；第一份语义化时间轴可能会在开始播放或歌词视图下次更新时到达。
5. Capture the iced KG Lyrics window in OBS if desired.
   如有需要，可在 OBS 中采集 iced 的 KG Lyrics 窗口。

Launching WeSing as a child avoids requesting access to an unrelated existing process. Attaching to an existing process remains an injector-only diagnostic mode.

将全民 K 歌作为子进程启动，可以避免请求访问无关的现有进程。附加到现有进程仍仅作为注入器的诊断模式使用。

## Compatibility and diagnostics / 兼容性与诊断

The current semantic reader is validated against WeSing/`KSongsUI.dll` version `2.21.176.1220`. A different binary may have a different internal lyric structure. The hook checks RTTI and function bytes and reports an unsupported-version error instead of installing a guessed detour.

当前语义读取器已针对全民 K 歌的 `KSongsUI.dll` 版本 `2.21.176.1220` 完成验证。其他二进制版本的内部歌词结构可能不同。钩子会检查 RTTI 和函数字节；遇到不受支持的版本时会报告错误，而不会安装猜测得出的跳转钩子。

Each launch creates a session directory under:

每次启动都会在以下位置创建会话目录：

```text
%LOCALAPPDATA%\kg-capture\logs\<timestamp>-<host-pid>-<session>
```

The directory is intentionally kept out of the UI. It contains:

该目录特意不在用户界面中显示，其中包含：

- `host.log`: session lifecycle, IPC handshake, warnings, and errors.
  `host.log`：会话生命周期、IPC 握手、警告和错误。
- `injector.log`: process launch, DLL injection, and `kg_capture_start` status.
  `injector.log`：进程启动、DLL 注入和 `kg_capture_start` 状态。
- `hook.log`: `KSongsUI.dll` loading, hook lifecycle, timeline changes, warnings, and errors.
  `hook.log`：`KSongsUI.dll` 加载、钩子生命周期、时间轴变化、警告和错误。

Logs default to `INFO` and above. Set `RUST_LOG=kg_capture=debug` before launching the host to include per-event records, callback counts, PE/RTTI details, timeline pointers, and accepted/rejected lyric diagnostics. When reporting a failure, include all three files; enable `DEBUG` first when investigating whether a WeSing view calls the known lyric update methods.

日志默认记录 `INFO` 及以上级别。启动宿主程序前设置 `RUST_LOG=kg_capture=debug`，可记录逐事件信息、回调次数、PE/RTTI 详情、时间轴指针以及歌词接受或拒绝的诊断信息。报告故障时请附上全部三个日志文件；若要调查全民 K 歌视图是否调用了已知的歌词更新方法，请先启用 `DEBUG`。

The hook never sends target-process pointers over IPC; all UTF-16 strings and timing values are copied into owned Rust values first, with line, word, string-length, and readable-memory bounds.

钩子绝不会通过 IPC 发送目标进程指针；所有 UTF-16 字符串和时间值都会先复制为 Rust 自有值，并对歌词行数、字数、字符串长度和可读内存范围进行限制。
