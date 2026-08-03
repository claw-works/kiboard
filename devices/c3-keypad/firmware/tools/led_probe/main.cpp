// 黄灯静态验证：GPIO9 一直保持低电平
//
// 真值表结果：GPIO9/GPIO2 的四种组合下黄灯全不亮，红灯只在 GPIO2=H 时亮。
//   红灯 = 正接到 GND、高电平亮，符合设计，已确认没问题。
//   黄灯四种组合全不亮 => 它这一路根本没有导通过。最可能是【LED 装反了】：
//   按反接方案接线时把长短脚搞反了，变成 3V3 -> 330R -> 阴极、阳极 -> GPIO9，
//   这样 GPIO9=L 时二极管反偏、GPIO9=H 时两端等电位，任何情况都不会亮。
//   其次可能是电阻或某根线插错孔、没接触。
//
// 本固件把 GPIO9 死死拉低、GPIO2 拉低（红灯灭），方便边接线边看：
//   接线正确 => 黄灯常亮。改完线立刻就能看到，不用等固件跑流程。
//
// 烧写：pio run -e ledprobe -t upload
#include <Arduino.h>
#include <Wire.h>
#include <Adafruit_GFX.h>
#include <Adafruit_SSD1306.h>

static Adafruit_SSD1306 oled(128, 64, &Wire, -1);
static bool oled_ok = false;

void setup() {
  pinMode(8, OUTPUT);
  digitalWrite(8, HIGH);  // 板载蓝灯关掉，免得干扰观察

  pinMode(2, OUTPUT);
  digitalWrite(2, LOW);   // 红灯灭

  pinMode(9, OUTPUT);
  digitalWrite(9, LOW);   // 黄灯：反接方案下低电平亮，接线正确就该常亮

  Serial.begin(115200);
  uint32_t t0 = millis();
  while (!Serial && millis() - t0 < 3000) {
    delay(10);
  }
  delay(200);
  Serial.println();
  Serial.println("=== 黄灯静态验证 ===");
  Serial.println("GPIO9 常低、GPIO2 常低、GPIO8 关。接线正确时黄灯应【常亮】，红蓝应【灭】。");
  Serial.println("改完线不用重刷，直接看灯。");

  Wire.begin(4, 5, 400000);
  oled_ok = oled.begin(SSD1306_SWITCHCAPVCC, 0x3C);
}

void loop() {
  // 电平已在 setup 里设好，这里只刷屏提示
  if (oled_ok) {
    oled.clearDisplay();
    oled.setTextColor(SSD1306_WHITE);
    oled.setTextSize(1);
    oled.setCursor(0, 0);
    oled.println("yellow LED static");
    oled.println();
    oled.setTextSize(2);
    oled.println("9 = LOW");
    oled.setTextSize(1);
    oled.println();
    oled.println("expect: YELLOW ON");
    oled.println("        red/blue off");
    oled.display();
  }
  delay(2000);
}
