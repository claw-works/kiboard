# 点阵字库

屏幕要显示 hub 下发的中文（agent 写的意图说明、审批提示），
而 Adafruit_GFX 内置字体只有 ASCII，中文会变成乱码。所以嵌了一套 12px 点阵字库。

| 文件 | 内容 | 大小 |
|---|---|---|
| `hzk12.bin` | GB2312 汉字 12×12，区位序（hi 0xA1-0xF7 / lo 0xA1-0xFE），每字 12 行 × 2 字节，高 12 位有效 | 192KB |
| `u2g.bin` | Unicode → GB2312 映射表，`{u16 unicode, u16 gb}` 小端、按 unicode 升序，供二分查找 | 29KB |

汉字字模来自文泉驿点阵宋体 9pt（原生 12px 手工点阵，不是缩放来的，笔画不粘连）。
生成脚本见 `../../kura-chan/kiiro-chan-firmware/tools/generate_wqy12.py`。

## ASCII 为什么不用字库

参考项目里还有一个 `asc12.bin`（6×12 ASCII），这里**故意没用**。

那份是从文泉驿 9pt 生成的，而文泉驿的 ASCII 是**比例字体**：`W`/`M`/`@`/`m` 原本 7px 宽，
生成脚本按列压缩硬塞进 6px 格子，笔画会粘在一起；`i`/`l` 这类窄字又在格子里空出一大块。
配上等宽的排布，看着就是"等宽但字体不对"。

改用 Adafruit_GFX 内置的 5×7 经典点阵（`drawChar()`），它本来就是为固定格子设计的等宽字模。
放进 12px 行时下移 3px，让字形底部落在第 9 行，与汉字基线（第 10 行）对齐。

**为什么坚持等宽**：屏上显示的是 shell 命令和路径，等宽能避免 `l`/`1`/`I`、`0`/`O` 混淆，
也让 `rm -rf /` 和 `rm -rf/` 的差别看得出来。对一个看错就出事的审批设备，
这是安全属性而不是审美偏好。

通过 `platformio.ini` 的 `board_build.embed_files` 编进 flash，
符号名形如 `_binary_data_hzk12_bin_start`。因为多了 ~230KB，
分区表改用 `huge_app.csv`（3MB 单 app、无 OTA）——我们本来就是 USB 刷机，不用 OTA。
