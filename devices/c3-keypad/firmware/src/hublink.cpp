#include "hublink.h"

#include <WebSocketsClient.h>
#include <WiFi.h>

#include "secrets.h"

namespace {
hublink::LineHandler gHandler = nullptr;

// --- 串口行缓冲 ---
char serialBuf[512];
size_t serialLen = 0;

// --- WebSocket ---
WebSocketsClient ws;
bool wsUp = false;
bool wsStarted = false;
bool justConnected = false;

void onWsEvent(WStype_t type, uint8_t *payload, size_t length) {
    switch (type) {
        case WStype_CONNECTED:
            wsUp = true;
            justConnected = true;  // 让 main 重发一次 hello
            Serial.println("[ws] connected");
            break;
        case WStype_DISCONNECTED:
            if (wsUp) Serial.println("[ws] disconnected");
            wsUp = false;
            break;
        case WStype_ERROR:
            Serial.printf("[ws] error: %.*s\n", static_cast<int>(length),
                          reinterpret_cast<char *>(payload));
            break;
        case WStype_TEXT:
            if (gHandler != nullptr && length > 0) {
                // payload 不保证以 \0 结尾，拷一份
                static char buf[1024];
                size_t n = length < sizeof(buf) - 1 ? length : sizeof(buf) - 1;
                memcpy(buf, payload, n);
                buf[n] = '\0';
                gHandler(buf);
            }
            break;
        default:
            break;
    }
}
}  // namespace

namespace hublink {

void begin(LineHandler handler) {
    gHandler = handler;
    Serial.begin(115200);
}

void loop() {
    // 周期诊断：确认这段代码在跑，以及看到的 WiFi 状态
    static uint32_t lastDiag = 0;
    if (millis() - lastDiag > 8000) {
        lastDiag = millis();
        Serial.printf("[ws] diag started=%d wsUp=%d wifi_status=%d\n", wsStarted ? 1 : 0,
                      wsUp ? 1 : 0, static_cast<int>(WiFi.status()));
        // 没连上时周期性做裸 TCP 探测，区分网络层问题和 WS 协议问题
        if (!wsUp && WiFi.status() == WL_CONNECTED) {
            WiFiClient probe;
            bool ok = probe.connect(HUB_HOST, HUB_PORT, 3000);
            Serial.printf("[ws] tcp probe %s -> %s:%u\n", ok ? "OK" : "FAILED", HUB_HOST,
                          HUB_PORT);
            probe.stop();
        }
    }

    // Wi-Fi 就绪后再启动 WS（只启动一次，之后由库自己重连）
    if (!wsStarted && WiFi.status() == WL_CONNECTED) {
        wsStarted = true;
        // 关掉 Wi-Fi 省电，否则延迟高、TCP 容易握手失败
        WiFi.setSleep(false);

        Serial.printf("[ws] connecting to %s:%u%s\n", HUB_HOST, HUB_PORT, HUB_WS_PATH);
        ws.onEvent(onWsEvent);
        ws.begin(HUB_HOST, HUB_PORT, HUB_WS_PATH);
        ws.setReconnectInterval(3000);
        // 30 秒无响应认为链路断开
        ws.enableHeartbeat(15000, 3000, 2);
    }
    if (wsStarted) ws.loop();

    // 串口按行读取
    while (Serial.available()) {
        char c = static_cast<char>(Serial.read());
        if (c == '\n' || c == '\r') {
            if (serialLen > 0) {
                serialBuf[serialLen] = '\0';
                if (gHandler != nullptr) gHandler(serialBuf);
                serialLen = 0;
            }
        } else if (serialLen < sizeof(serialBuf) - 1) {
            serialBuf[serialLen++] = c;
        } else {
            serialLen = 0;  // 超长丢弃
        }
    }
}

void send(JsonDocument &doc) {
    char out[512];
    size_t n = serializeJson(doc, out, sizeof(out));
    if (n == 0) return;

    // 只走一条链路，避免 hub 收到重复消息（一次按键被算作两次）
    if (wsUp) {
        ws.sendTXT(out, n);
    } else {
        Serial.write(reinterpret_cast<uint8_t *>(out), n);
        Serial.write('\n');
    }
}

bool takeJustConnected() {
    bool v = justConnected;
    justConnected = false;
    return v;
}

bool wsConnected() { return wsUp; }

}  // namespace hublink
