#pragma once
#include <Adafruit_GFX.h>

// 中文点阵渲染：UTF-8 -> Unicode -> GB2312 -> 12px 点阵
//
// 为什么需要：屏幕要显示 hub 下发的中文（agent 写的意图说明、审批提示），
// 而 Adafruit_GFX 内置字体只有 ASCII，中文直接变乱码。
//
// 字库嵌在 flash 里（见 data/README.md），汉字 12×12、ASCII 6×12。
// 12px 在 128×64 上是个合适的取舍：一行 10 个汉字或 21 个 ASCII，竖向能放 5 行。
// 内置字体的 6×8 更省地方但画不了中文；16px 字库更好看但一行只剩 8 个字。
namespace cnfont {

constexpr int LINE_H = 12;   // 字高
constexpr int ASCII_W = 6;   // ASCII 字宽
constexpr int CJK_W = 12;    // 汉字字宽

// 画一行（不换行）。opaque=true 时连背景一起画，false 只画笔画（叠在已有内容上）。
// 返回画出的像素宽度。
int drawString(Adafruit_GFX &gfx, int x, int y, const char *utf8, uint16_t fg,
               bool opaque = false, uint16_t bg = 0);

// 自动折行。超过 maxW 像素折行，从第 skip 行开始画，最多画 maxLines 行。
// step 是行间步进（含行距）。返回【折行后的总行数】——调用方据此判断有没有内容被藏起来。
//
// 为什么要 skip：屏幕只有几行，长命令一定放不下。截断是危险的（看不到的部分可能正是
// 要命的那段），所以支持滚动，并让调用方知道总共有多少行好画出"还有内容"的提示。
int drawWrapped(Adafruit_GFX &gfx, int x, int y, int maxW, int maxLines, int step,
                const char *utf8, uint16_t fg, bool opaque = false, uint16_t bg = 0,
                int skip = 0);

// 量宽度，不画。用于居中。
int textWidth(const char *utf8);

// 按像素宽度截断，尾部加 ~ 表示还有内容。返回值指向内部静态缓冲，用完即取。
const char *ellipsize(const char *utf8, int maxW);

}  // namespace cnfont
