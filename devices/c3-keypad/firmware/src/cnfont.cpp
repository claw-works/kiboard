#include "cnfont.h"

#include <string.h>

// platformio.ini 的 board_build.embed_files 把字库编进 flash，符号名由文件路径生成
extern const uint8_t HZK12[] asm("_binary_data_hzk12_bin_start");
extern const uint8_t U2G[] asm("_binary_data_u2g_bin_start");
extern const uint8_t U2G_END[] asm("_binary_data_u2g_bin_end");

// ASCII 不用字库里的 asc12：那份是从文泉驿 9pt（比例字体）挤进 6px 格子生成的，
// W/M/@/m 原本 7px 宽被按列压缩，笔画粘在一起；i/l 又在格子里空出一大块。
// 等宽的排布配比例的字形，看着就是别扭。
//
// 改用 Adafruit_GFX 内置的 5x7 经典点阵——它本来就是为固定格子设计的等宽字模，
// 通过公开的 drawChar() 调用，不必把字模数据复制过来。
//
// 为什么坚持等宽：屏上显示的是 shell 命令和路径，等宽能避免 l/1/I、0/O 混淆，
// 也让 "rm -rf /" 和 "rm -rf/" 的差别看得出来。对一个看错就出事的审批设备，
// 这是安全属性而不是审美偏好。

namespace {

// UTF-8 解码一个码点，*s 前进。非法字节跳过并返回替换字符
uint32_t utf8Next(const char **s) {
    const uint8_t *p = reinterpret_cast<const uint8_t *>(*s);
    uint32_t cp;
    int len;
    if (p[0] < 0x80) {
        cp = p[0];
        len = 1;
    } else if ((p[0] & 0xE0) == 0xC0) {
        cp = p[0] & 0x1F;
        len = 2;
    } else if ((p[0] & 0xF0) == 0xE0) {
        cp = p[0] & 0x0F;
        len = 3;
    } else if ((p[0] & 0xF8) == 0xF0) {
        cp = p[0] & 0x07;
        len = 4;
    } else {
        (*s)++;
        return 0xFFFD;
    }
    for (int i = 1; i < len; i++) {
        if ((p[i] & 0xC0) != 0x80) {  // 截断的多字节序列
            *s += i;
            return 0xFFFD;
        }
        cp = (cp << 6) | (p[i] & 0x3F);
    }
    *s += len;
    return cp;
}

struct U2GEntry {
    uint16_t unicode;
    uint16_t gb;
};

// Unicode -> GB2312。表按 unicode 升序，二分查。0 表示字库里没有
uint16_t toGb2312(uint32_t cp) {
    if (cp > 0xFFFF) {
        return 0;
    }
    const U2GEntry *table = reinterpret_cast<const U2GEntry *>(U2G);
    int lo = 0;
    int hi = static_cast<int>((U2G_END - U2G) / sizeof(U2GEntry)) - 1;
    while (lo <= hi) {
        int mid = (lo + hi) / 2;
        uint16_t u = table[mid].unicode;
        if (u == cp) {
            return table[mid].gb;
        }
        if (u < cp) {
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    return 0;
}

void drawCjk(Adafruit_GFX &gfx, int x, int y, uint16_t gb, uint16_t fg, bool opaque,
             uint16_t bg) {
    uint8_t hi = gb >> 8;
    uint8_t lo = gb & 0xFF;
    if (hi < 0xA1 || lo < 0xA1) {
        return;
    }
    size_t off = (static_cast<size_t>(hi - 0xA1) * 94 + (lo - 0xA1)) * 24;
    for (int row = 0; row < cnfont::LINE_H; row++) {
        uint16_t bits = (HZK12[off + row * 2] << 8) | HZK12[off + row * 2 + 1];
        for (int col = 0; col < cnfont::CJK_W; col++) {
            bool on = bits & (0x8000 >> col);
            if (on) {
                gfx.drawPixel(x + col, y + row, fg);
            } else if (opaque) {
                gfx.drawPixel(x + col, y + row, bg);
            }
        }
    }
}

// 5x7 字模放进 12px 行里：汉字的基线在第 10 行（字库按 ascent=10 生成），
// 让 7px 高的字形底部落在第 9 行，即 y+3 起画，这样中英文混排时基线是齐的。
constexpr int ASCII_Y_OFFSET = 3;

void drawAscii(Adafruit_GFX &gfx, int x, int y, uint8_t ch, uint16_t fg, bool opaque,
               uint16_t bg) {
    // Adafruit_GFX 的约定：bg == fg 表示透明，不填背景
    gfx.drawChar(x, y + ASCII_Y_OFFSET, ch, fg, opaque ? bg : fg, 1);
}

// 字库里没收录的字用两个 ? 占位，宽度和汉字一致，不至于让后面的字错位
void drawMissing(Adafruit_GFX &gfx, int x, int y, uint16_t fg, bool opaque, uint16_t bg) {
    drawAscii(gfx, x, y, '?', fg, opaque, bg);
    drawAscii(gfx, x + cnfont::ASCII_W, y, '?', fg, opaque, bg);
}

}  // namespace

namespace cnfont {

int drawString(Adafruit_GFX &gfx, int x, int y, const char *utf8, uint16_t fg, bool opaque,
               uint16_t bg) {
    int cx = x;
    const char *s = utf8;
    while (*s) {
        uint32_t cp = utf8Next(&s);
        if (cp < 0x80) {
            drawAscii(gfx, cx, y, static_cast<uint8_t>(cp), fg, opaque, bg);
            cx += ASCII_W;
        } else {
            uint16_t gb = toGb2312(cp);
            if (gb) {
                drawCjk(gfx, cx, y, gb, fg, opaque, bg);
            } else {
                drawMissing(gfx, cx, y, fg, opaque, bg);
            }
            cx += CJK_W;
        }
    }
    return cx - x;
}

int drawWrapped(Adafruit_GFX &gfx, int x, int y, int maxW, int maxLines, int step,
                const char *utf8, uint16_t fg, bool opaque, uint16_t bg, int skip) {
    int cx = x;
    int line = 0;
    const char *s = utf8;
    // 窗口外的行也要走一遍折行逻辑，否则数不出总行数、也算不准该从哪断
    while (*s) {
        uint32_t cp = utf8Next(&s);
        if (cp == '\n') {
            cx = x;
            line++;
            continue;
        }
        int w = (cp < 0x80) ? ASCII_W : CJK_W;
        if (cx + w > x + maxW) {
            cx = x;
            line++;
        }
        int vis = line - skip;
        if (vis >= 0 && vis < maxLines) {
            int dy = y + vis * step;
            if (cp < 0x80) {
                drawAscii(gfx, cx, dy, static_cast<uint8_t>(cp), fg, opaque, bg);
            } else {
                uint16_t gb = toGb2312(cp);
                if (gb) {
                    drawCjk(gfx, cx, dy, gb, fg, opaque, bg);
                } else {
                    drawMissing(gfx, cx, dy, fg, opaque, bg);
                }
            }
        }
        cx += w;
    }
    return line + 1;
}

int textWidth(const char *utf8) {
    int w = 0;
    const char *s = utf8;
    while (*s) {
        uint32_t cp = utf8Next(&s);
        w += (cp < 0x80) ? ASCII_W : CJK_W;
    }
    return w;
}

const char *ellipsize(const char *utf8, int maxW) {
    static char buf[96];
    int w = 0;
    const char *s = utf8;
    const char *lastFit = utf8;
    while (*s) {
        const char *before = s;
        uint32_t cp = utf8Next(&s);
        int cw = (cp < 0x80) ? ASCII_W : CJK_W;
        // 留出一个 ~ 的位置
        if (w + cw > maxW - ASCII_W) {
            lastFit = before;
            break;
        }
        w += cw;
        lastFit = s;
    }
    if (!*lastFit) {  // 整串都放得下
        return utf8;
    }
    size_t n = static_cast<size_t>(lastFit - utf8);
    if (n > sizeof(buf) - 2) {
        n = sizeof(buf) - 2;
    }
    memcpy(buf, utf8, n);
    buf[n] = '~';
    buf[n + 1] = '\0';
    return buf;
}

}  // namespace cnfont
