// OLED 上电探测：扫 I2C 总线 -> 报地址 -> 尝试按 SSD1306 点亮
//
// 只用于杜邦线阶段的硬件确认，不参与正式固件。
// 烧写：pio run -e oledprobe -t upload && pio device monitor
#include <Arduino.h>
#include <Wire.h>
#include <Adafruit_GFX.h>
#include <Adafruit_SSD1306.h>

// v3 载板：OLED 走 I2C
static constexpr int PIN_SDA = 4;
static constexpr int PIN_SCL = 5;

// 分辨率未确认，先按最常见的 128x64 试
static constexpr int OLED_W = 128;
static constexpr int OLED_H = 64;

static Adafruit_SSD1306 oled(OLED_W, OLED_H, &Wire, -1);

// 扫到的第一个地址
static uint8_t found_addr = 0;
static int found_count = 0;

static void scanBus() {
  Serial.println("[i2c] scanning 0x01..0x7E on SDA=GPIO4 SCL=GPIO5 ...");
  for (uint8_t addr = 1; addr < 0x7F; addr++) {
    Wire.beginTransmission(addr);
    uint8_t err = Wire.endTransmission();
    if (err == 0) {
      Serial.printf("[i2c] FOUND device at 0x%02X\n", addr);
      found_count++;
      if (found_addr == 0) {
        found_addr = addr;
      }
    }
  }
  if (found_count == 0) {
    Serial.println("[i2c] no device found -- 检查 VCC/GND/SDA/SCL 四根线");
  } else {
    Serial.printf("[i2c] %d device(s), using 0x%02X\n", found_count, found_addr);
  }
}

void setup() {
  Serial.begin(115200);
  // C3 原生 USB CDC 需要等主机枚举，否则前几行日志会丢
  uint32_t t0 = millis();
  while (!Serial && millis() - t0 < 3000) {
    delay(10);
  }
  delay(200);
  Serial.println();
  Serial.println("=== kiboard OLED probe ===");

  Wire.begin(PIN_SDA, PIN_SCL, 400000);
  scanBus();

  if (found_addr == 0) {
    return;
  }

  if (!oled.begin(SSD1306_SWITCHCAPVCC, found_addr)) {
    Serial.println("[oled] SSD1306 begin() FAILED -- 可能是 SH1106，或分辨率不对");
    return;
  }
  Serial.println("[oled] SSD1306 begin() ok, drawing test pattern");

  oled.clearDisplay();
  // 整屏边框：能判断实际可见高度是 64 还是 32
  oled.drawRect(0, 0, OLED_W, OLED_H, SSD1306_WHITE);
  oled.setTextColor(SSD1306_WHITE);

  oled.setTextSize(2);
  oled.setCursor(6, 6);
  oled.print("kiboard");

  oled.setTextSize(1);
  oled.setCursor(6, 26);
  oled.printf("I2C 0x%02X", found_addr);
  oled.setCursor(6, 38);
  oled.printf("%dx%d assumed", OLED_W, OLED_H);

  // 底部一条实心横杠，贴着 y=63：如果看不到，说明屏其实是 128x32
  oled.fillRect(6, OLED_H - 10, OLED_W - 12, 6, SSD1306_WHITE);

  oled.display();
  Serial.println("[oled] done -- 屏上应看到: 边框 + kiboard + 地址 + 底部横杠");
}

void loop() {
  static uint32_t last = 0;
  static bool on = true;
  if (millis() - last < 1000) {
    return;
  }
  last = millis();
  on = !on;
  Serial.printf("[alive] %lus i2c=0x%02X\n", millis() / 1000, found_addr);
  if (found_addr != 0) {
    // 反色闪烁，肉眼确认屏在持续刷新而不是残影
    oled.invertDisplay(on);
  }
}
