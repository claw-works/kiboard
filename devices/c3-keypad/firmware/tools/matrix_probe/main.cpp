// 4x4 矩阵键盘 + OLED 上电探测
//
// 扫描策略：只把当前行设为 OUTPUT 拉高，其余三行设为 INPUT_PULLDOWN，列脚 INPUT_PULLDOWN。
// 不用「其余行 OUTPUT 拉低」——同一列上两个不同行的键同时按下会造成推挽对推挽短路。
// 也不能用纯 INPUT——GPIO2 是 strapping 脚，纯 INPUT 会悬空成高（实测）。
//
// 屏上显示最近按键，串口打印 R{行}C{列} 便于和一体板丝印对照。
// 烧写：pio run -e matrixprobe -t upload
#include <Arduino.h>
#include <Wire.h>
#include <Adafruit_GFX.h>
#include <Adafruit_SSD1306.h>

static constexpr int PIN_SDA = 4;
static constexpr int PIN_SCL = 5;
static constexpr uint8_t OLED_ADDR = 0x3C;  // 已实测确认
static constexpr int OLED_W = 128;
static constexpr int OLED_H = 64;

static Adafruit_SSD1306 oled(OLED_W, OLED_H, &Wire, -1);
static bool oled_ok = false;

static constexpr int ROWS[4] = {0, 1, 21, 3};     // R1..R4（R3 从 GPIO2 挪到 GPIO21）
static constexpr int COLS[4] = {6, 7, 10, 20};    // C1..C4

// 已验证的消抖参数：关键是事件级冷却，不是延长稳定时间
static constexpr uint32_t DEBOUNCE_MS = 30;
static constexpr uint32_t EVENT_COOLDOWN_MS = 250;

struct KeyState {
  bool stable;         // 当前确认状态
  bool pending;        // 待确认的新电平
  uint32_t since;      // pending 起始时刻
  uint32_t last_event; // 上次上报事件的时刻
};

static KeyState keys[4][4];
static uint32_t press_total = 0;
static char last_label[16] = "-";

// 非扫描行的空闲态用 INPUT_PULLDOWN：既有确定电平，又足够弱（45k），
// 同列多键按下时被扫描行只需灌约 73uA，不会出现推挽对推挽短路。
//
// 注意 R3 已从 GPIO2 挪到 GPIO21。实测 SuperMini 在三个 strapping 脚
// GPIO2/8/9 上都带板载上拉，强到 45k 内部下拉压不住（拔掉所有外部接线后
// 仍读到高），因此这三个脚不能用作任何需要判读电平的输入，只能当输出。
// 当初 GPIO2 作行线时，节点被顶在 2.3~2.7V 的不确定带里，
// GPIO6/7 判低而 GPIO10/20 判高，表现为「第三排只有后两个键出错」。
static void allRowsIdle() {
  for (int r = 0; r < 4; r++) {
    pinMode(ROWS[r], INPUT_PULLDOWN);
  }
}

// 读取一行：拉高该行，读四列，然后还原为下拉空闲态
static void readRow(int r, bool out[4]) {
  pinMode(ROWS[r], OUTPUT);
  digitalWrite(ROWS[r], HIGH);
  delayMicroseconds(50);  // 等电平稳定（杜邦线有寄生电容）
  for (int c = 0; c < 4; c++) {
    out[c] = digitalRead(COLS[c]) == HIGH;
  }
  pinMode(ROWS[r], INPUT_PULLDOWN);
}

// 上电自检：
//  1) 行脚设为纯 INPUT 时是否悬空成高 —— 就是上面那个坑，报出来免得下次再查一遍
//  2) 行脚下拉、无按键时列脚应全为低 —— 为高说明列线接到了电源轨
static void checkIdle() {
  bool floaty = false;
  for (int r = 0; r < 4; r++) {
    pinMode(ROWS[r], INPUT);
  }
  delay(5);
  for (int r = 0; r < 4; r++) {
    if (digitalRead(ROWS[r]) == HIGH) {
      Serial.printf("[selftest] R%d (GPIO%d) 纯 INPUT 时悬空为高，已靠内部下拉压住\n",
                    r + 1, ROWS[r]);
      floaty = true;
    }
  }
  if (!floaty) {
    Serial.println("[selftest] 四个行脚纯 INPUT 时均为低");
  }

  allRowsIdle();
  delay(5);
  bool bad = false;
  for (int c = 0; c < 4; c++) {
    if (digitalRead(COLS[c]) == HIGH) {
      Serial.printf("[warn] C%d (GPIO%d) 空闲时为高 -- 可能接到了 3V3 或行线上\n",
                    c + 1, COLS[c]);
      bad = true;
    }
  }
  if (!bad) {
    Serial.println("[selftest] 空闲电平正常：C1~C4 全部为低");
  }
}

static void drawScreen() {
  if (!oled_ok) {
    return;
  }
  oled.clearDisplay();
  oled.setTextColor(SSD1306_WHITE);

  oled.setTextSize(1);
  oled.setCursor(0, 0);
  oled.print("matrix probe");

  oled.setTextSize(3);
  oled.setCursor(0, 16);
  oled.print(last_label);

  oled.setTextSize(1);
  oled.setCursor(0, 48);
  oled.printf("count %lu", press_total);

  // 右侧 4x4 点阵实时反映按下状态
  const int ox = 88, oy = 16, step = 10;
  for (int r = 0; r < 4; r++) {
    for (int c = 0; c < 4; c++) {
      int x = ox + c * step;
      int y = oy + r * step;
      if (keys[r][c].stable) {
        oled.fillRect(x, y, 7, 7, SSD1306_WHITE);
      } else {
        oled.drawRect(x, y, 7, 7, SSD1306_WHITE);
      }
    }
  }
  oled.display();
}

void setup() {
  Serial.begin(115200);
  uint32_t t0 = millis();
  while (!Serial && millis() - t0 < 3000) {
    delay(10);
  }
  delay(200);
  Serial.println();
  Serial.println("=== kiboard matrix probe ===");
  Serial.printf("rows R1-R4 = GPIO %d/%d/%d/%d\n", ROWS[0], ROWS[1], ROWS[2], ROWS[3]);
  Serial.printf("cols C1-C4 = GPIO %d/%d/%d/%d\n", COLS[0], COLS[1], COLS[2], COLS[3]);

  Wire.begin(PIN_SDA, PIN_SCL, 400000);
  oled_ok = oled.begin(SSD1306_SWITCHCAPVCC, OLED_ADDR);
  Serial.printf("[oled] %s\n", oled_ok ? "ok" : "FAILED");

  for (int c = 0; c < 4; c++) {
    pinMode(COLS[c], INPUT_PULLDOWN);
  }
  checkIdle();  // 内部会把行脚置为下拉空闲态

  uint32_t now = millis();
  for (int r = 0; r < 4; r++) {
    for (int c = 0; c < 4; c++) {
      keys[r][c] = {false, false, now, 0};
    }
  }

  drawScreen();
  Serial.println("[ready] 按键试试，串口会打印 R?C? —— 请对照丝印告诉我映射");
}

void loop() {
  uint32_t now = millis();
  bool changed = false;

  for (int r = 0; r < 4; r++) {
    bool raw[4];
    readRow(r, raw);
    for (int c = 0; c < 4; c++) {
      KeyState &k = keys[r][c];
      if (raw[c] != k.pending) {
        k.pending = raw[c];
        k.since = now;
        continue;
      }
      if (k.pending == k.stable) {
        continue;
      }
      if (now - k.since < DEBOUNCE_MS) {
        continue;
      }
      if (now - k.last_event < EVENT_COOLDOWN_MS) {
        continue;
      }
      k.stable = k.pending;
      k.last_event = now;
      changed = true;
      if (k.stable) {
        press_total++;
        snprintf(last_label, sizeof(last_label), "R%dC%d", r + 1, c + 1);
        Serial.printf("[key] R%dC%d (GPIO%d x GPIO%d) PRESS   #%lu\n",
                      r + 1, c + 1, ROWS[r], COLS[c], press_total);
      } else {
        Serial.printf("[key] R%dC%d RELEASE\n", r + 1, c + 1);
      }
    }
  }

  if (changed) {
    drawScreen();
  }

  static uint32_t last_beat = 0;
  if (now - last_beat >= 5000) {
    last_beat = now;
    Serial.printf("[alive] %lus presses=%lu\n", now / 1000, press_total);
  }

  delay(2);
}
