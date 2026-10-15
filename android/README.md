<!--
  PureVox — AI 麦克风降噪工具
  Copyright (C) 2024-2026 a2heng <752848283@qq.com>

  PureVox is licensed under the GNU General Public License v3.0 or
  later (GPL-3.0-or-later).  See LICENSE for details.

  The built-in AI models are NOT covered by the GPL; they are the
  property of a2heng and may only be used with PureVox under
  authorization.  See MODEL-LICENSE.md for details.

  SPDX-License-Identifier: GPL-3.0-or-later
-->

# PureVox Android 客户端

手机 ⇄ 电脑的第二端：**手机麦克风当电脑麦克风**、**手机扬声器当电脑音箱**、
**手机键盘当电脑全尺寸键盘**。三条能力共用电脑端的一个 WebSocket 连接。

包名 `com.purevox.mic`，应用名 PureVox。

## 协议（与 `src-tauri/src/net/proto.rs` 一一对应）

| 项 | 值 |
| --- | --- |
| 传输 | 明文 `ws://`，端口 **59123**，路径 **`/ws`** |
| 自检 | `GET http://<host>:59123/health`（不需要连上 ws） |
| 握手 | 客户端 → `{"t":"hello","proto":1}`；电脑回 `{"t":"ready",…}` |
| 音频 | **二进制帧 = 一个 Opus 包（无头）**，48 kHz 单声道 |
| 控制 | 文本帧 = JSON，判别字段 `t` |

客户端 → 电脑：`hello` / `sub` / `text` / `key` / `ping` / `rtt`。
电脑 → 客户端：`ready` / `pong` / `err`（`err` 原文一律显示给用户，不静默）。

**收到 `ready` 之前不推流、不播放。** 远程输入在电脑端默认关闭，未开启时发
`key` / `text` 会收到 `err`，本客户端把它显示在状态区的红字行。

## 代码结构（每个文件的职责在文件头注释里）

```
app/src/main/java/com/purevox/mic/
├── Proto.kt                     协议常量与 JSON 拼装 / 解析（proto.rs 的镜像）
├── MainActivity.kt              单界面：连接、三条能力编排、实体键拦截、IME 文本同步、状态读数
├── net/
│   ├── WsClient.kt              WebSocket 连接、hello/ping/rtt、二进制包收发、RTT 计算
│   └── HealthProbe.kt           GET /health 自检（不依赖 ws 已连上）
├── audio/
│   ├── OpusSupport.kt           audio/opus 能力探测（编解码器名 + 输入 PCM 格式）
│   ├── OpusEncoder.kt           上行编码（输出驱动，帧长不假设 10 ms）
│   ├── OpusDecoder.kt           下行解码（输出帧长按实际样本数，不假设）
│   ├── MicUplink.kt             AudioRecord 采集 + 编码线程
│   └── SpeakerDownlink.kt       包队列 + 解码线程 + 样本环形缓冲 + AudioTrack 写入线程
└── service/
    └── CaptureService.kt        麦克风前台服务（通知 + WakeLock，Android 14+ microphone 类型）
```

## 没有的东西（刻意的）

* **没有 JNI / CMake / NDK / C 代码**：Opus 编解码全部走系统 `MediaCodec`（`audio/opus`）。
* **没有 TLS 与证书信任绕过**：新版协议就是明文 `ws://` + `usesCleartextTraffic="true"`。

## 构建

```bash
cd android
./gradlew :app:assembleDebug      # 产物 app/build/outputs/apk/debug/app-debug.apk
./gradlew :app:assembleRelease    # 产物 app/build/outputs/apk/release/app-release.apk
```

需要 JDK 17 与 Android SDK（`compileSdk 34`）。`gradle/wrapper/gradle-wrapper.jar` 已入库，
不要替换；`gradle-wrapper.properties` 钉的是 Gradle 8.9（AGP 8.7.3 的最低要求）。

### release 签名

**凭据一律走环境变量、keystore 放在仓库外面**，仓库里没有 `keystore.properties` 也没有
`*.jks` —— 于是不存在「忘了写 .gitignore 把私钥提交上去」这种坑。

| 变量 | 内容 |
| --- | --- |
| `PV_KEYSTORE` | keystore 文件的**绝对路径**（PKCS12） |
| `PV_KEY_ALIAS` | 密钥别名 |
| `PV_KEY_PASS` | keystore 口令与私钥口令（同一个） |
| `PV_REQUIRE_SIGNING` | `1` = 缺凭据**直接失败**；不设 = 产出未签名的包 |

```bash
export PV_KEYSTORE="$HOME/.purevox/signing/purevox-release.p12"
export PV_KEY_ALIAS=purevox
export PV_KEY_PASS='…'
./gradlew :app:assembleRelease
```

### 签名方案（逐条写死，不靠 AGP 默认值）

| 方案 | 取值 | 依据 |
| --- | --- | --- |
| v1（JAR） | **关** | 只对 `minSdk < 24` 有意义；本项目 minSdk=24，开着只是白搭一份 `META-INF` |
| v2 | **开** | 必需项。整包哈希，比 v1 的逐条目哈希更难伪造 |
| v3 | **开** | 目前唯一带密钥轮换能力的方案（OWASP：「至少 v2 或 v3，想要轮换就用 v3」） |
| v4 | 不做 | 只产出独立的 `.idsig`（供 adb 增量安装），自身零安全收益，还多一个附件 |

密钥本身：PKCS12（JKS 已过时）、RSA-4096（≥ 官方要求的 2048）、有效期 30 年
（官方建议 ≥ 25 年，Play 额外要求有效期晚于 2033-10-22）。

**开了 v3 ≠ 应该去轮换密钥。** proof-of-rotation 只有 **Android 13+（API 33）**认，
Android 12 及更早会忽略它继续用原密钥 —— 本项目 `minSdk = 24`，一旦轮换，
那一大片用户会因为新密钥不被认识而**装不上**。所以正确做法是：开 v3 当保险，**把密钥守住**。

**没设凭据时 `assembleRelease` 仍能出包，但那是未签名的 —— Android 不允许安装未签名包。**
本机调试无所谓，CI 一律设 `PV_REQUIRE_SIGNING=1`，缺 secret 就当场红，不产出装不上的版本
（发出去无法补救，重打要重发一版）。CI 的三个 secret 与生成方式见
`~/.purevox/signing/keystore-info.txt`。

## 依赖 Android 版本的行为

| 行为 | 起始版本 |
| --- | --- |
| `AudioRecord` / `AudioTrack` 的 `ENCODING_PCM_FLOAT` | API 23（minSdk 24 覆盖） |
| `MediaFormat` 的 `opus-frame-duration-us` | API 29；不认这个 key 时自动降级 |
| `startForeground(…, FOREGROUND_SERVICE_TYPE_MICROPHONE)` | API 29；24~28 用旧的双参重载 |
| `POST_NOTIFICATIONS` 运行时申请 | API 33 |