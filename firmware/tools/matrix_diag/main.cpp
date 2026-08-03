// 找出 R3 那根线到底插在哪个 GPIO 上（v11）
//
// 已知：GPIO21 自回读正常（驱动高读回 1、驱动低读回 0），引脚本身没问题，
//       所以第三排没反应不是引脚被占用，而是这根线没接到该接的地方。
//
// 做法：不假设线在哪。请按住物理第三排任一键，然后逐个把候选 GPIO 驱动为高，
//       看有没有哪个列脚跟着变高。哪个 GPIO 让某列变高，线就在那个 GPIO 上。
//       全都没反应 = 这根线没插到任何 GPIO 上（松了、插错排、或插进了没用的孔）。
//
// 候选：GPIO0/1/2/3/8/9/21（行脚 + 备用脚）。跳过 4/5（I2C）与 6/7/10/20（列脚本身）。
//
// 请【按住】物理第三排最左边那个键不放。
// 烧写：pio run -e matrixdiag -t upload
#include <Arduino.h>
#include <Wire.h>
#include <Adafruit_GFX.h>
#include <Adafruit_SSD1306.h>

static Adafruit_SSD1306 oled(128, 64, &Wire, -1);
static bool oled_ok = false;

static constexpr int COLS[4] = {6, 7, 10, 20};
static constexpr int CAND[] = {0, 1, 2, 3, 8, 9, 21};
static constexpr int NCAND = sizeof(CAND) / sizeof(CAND[0]);

static void candIdle() {
  for (int i = 0; i < NCAND; i++) {
    pinMode(CAND[i], INPUT_PULLDOWN);
  }
}

void setup() {
  Serial.begin(115200);
  uint32_t t0 = millis();
  while (!Serial && millis() - t0 < 3000) {
    delay(10);
  }
  delay(200);
  Serial.println();
  Serial.println("=== R3 线在哪个 GPIO 上 (v11) ===");
  Serial.println("请【按住】物理第三排最左边那个键不放");
  Serial.println("逐个驱动候选 GPIO 为高，列出跟着变高的列脚");

  Wire.begin(4, 5, 400000);
  oled_ok = oled.begin(SSD1306_SWITCHCAPVCC, 0x3C);
  for (int c = 0; c < 4; c++) {
    pinMode(COLS[c], INPUT_PULLDOWN);
  }
  candIdle();
}

void loop() {
  char line[220];
  int n = 0;
  int hits = 0;
  char hit_names[64];
  int hn = 0;

  for (int i = 0; i < NCAND; i++) {
    candIdle();
    pinMode(CAND[i], OUTPUT);
    digitalWrite(CAND[i], HIGH);
    delayMicroseconds(200);
    char bits[5];
    for (int c = 0; c < 4; c++) {
      bits[c] = digitalRead(COLS[c]) == HIGH ? '1' : '0';
    }
    bits[4] = 0;
    pinMode(CAND[i], INPUT_PULLDOWN);

    n += snprintf(line + n, sizeof(line) - n, "%d:%s ", CAND[i], bits);
    if (strcmp(bits, "0000") != 0) {
      hits++;
      hn += snprintf(hit_names + hn, sizeof(hit_names) - hn, "GPIO%d ", CAND[i]);
    }
  }
  candIdle();

  if (hits == 0) {
    Serial.printf("%s | 没有任何 GPIO 有反应 -> R3 线没接上\n", line);
  } else {
    hit_names[hn] = 0;
    Serial.printf("%s | 有反应: %s\n", line, hit_names);
  }

  if (oled_ok) {
    oled.clearDisplay();
    oled.setTextColor(SSD1306_WHITE);
    oled.setTextSize(1);
    oled.setCursor(0, 0);
    oled.println("hold row3 key");
    oled.println("gpio:C1C2C3C4");
    for (int i = 0; i < NCAND; i++) {
      // 屏幕上重跑一遍太慢，这里只显示串口那一行的前半段
    }
    oled.println(line);
    oled.println();
    oled.println(hits == 0 ? "R3 wire NOT connected" : "found");
    oled.display();
  }

  delay(500);
}
