#pragma once
#include <Arduino.h>
#include <ArduinoJson.h>

// 传输层：设备与 hub 之间的消息通道。
// 串口和 WebSocket 并存——串口用于开发调试（能刷固件看日志），
// WS 用于日常无线使用。收到的消息不区分来源，发出的消息走所有已连通道。
namespace hublink {

using LineHandler = void (*)(const char *line);

void begin(LineHandler handler);
void loop();

// 广播到所有可用通道
void send(JsonDocument &doc);

bool wsConnected();

// 链路刚建立时返回一次 true。设备在 setup() 里发过的 hello 只走了当时可用的链路，
// WS 后连上时 hub 那边没收到过 hello（只看到 pong），status 里的 firmware/keys 会是空的。
// main 用这个标志在链路建立后重新自报一次。
bool takeJustConnected();

}  // namespace hublink
