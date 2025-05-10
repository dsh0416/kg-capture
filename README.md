# KG Capture

This project is designed to capture the Lyrics screen of WeSing (全民K歌).

本项目是为了捕获全民K歌的歌词画面。

WeSing set `WS_EX_LAYERED`, which causes the GDI or DXGI to not capture the window correctly.
Fortunately, `PrintWindow` with the `PRINT_WINDOW_FLAGS(2)` flag works.

全民K歌设置了 `WS_EX_LAYERED` 的窗口参数，这导致 GDI 或 DXGI 不能正确获取窗口。好消息是，`PrintWindow` 里带上 `PRINT_WINDOW_FLAGS(2)` 参数的话是可以工作的。

Meanwhile, WeSing would try to cover the window with white after receiving `WM_PAINT` message, considering vertical sync, white may not fill every row.

同时，全民K歌会在收到 `WM_PAINT` 窗口消息后用白屏盖住自己，考虑到垂直同步影响，甚至不是每行同时触发这样的机制。

This project tries to create a separated window with captured WeSing lyrics, and the new window could be captured by OBS or other capturing softwares.

本项目会创建一个独立的窗口，窗口内容是捕获的全民K歌歌词，并且该窗口可以被 OBS 或其他捕获软件捕获。

## How-to

1. Start WeSing. 打开全民K歌
2. Reuqest at least one song to create the lyrics window. 至少点一首歌以启动歌词窗口
3. Open this software. 打开本软件
