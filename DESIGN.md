# PureVox 设计（Tauri 3 / Rust）

本文件是 Tauri 主线（`src-tauri/`）的顶层设计：分层、数据单位与不变量、**列（Column）** 模型、
Stage 契约、SessionPlan、设备生命周期（含刷新语义）、模型生命周期、内存管理、扩展指南。
**实现与本文件冲突时以本文件为准**，改动设计先改这里再改代码。

> 旧实现（Python + Tk）冻结在 `legacy-v2026.09.30.1944/`。本文**不照搬**它：只保留骨架
> （10 ms hop / Stage 契约 / 单一实现路径 / 有界缓冲），并按下面的**列模型**重组链路。

## 0. 设计取向

- **显式优于隐式**：刷新设备、开始/停止、模型选择都由用户明确点击触发；不做后台自动重连、
  不自动改选设备、不做空闲自动卸载。
- **少状态**：流只有「运行中 / 已停止」两态；异常统一为「不可用 + 原因字符串」，无状态机、无重试。
- **列与列完全独立**：没有跨列混合，没有全局混音器。

---

## 1. 分层与目录

| 层 | 位置 | 职责 | 禁止 |
| --- | --- | --- | --- |
| L0 平台 | `src-tauri/src/audio/`（capture / playback / fanout）、`devices.rs` | 设备枚举与打开、流读写、时钟主控 | 不做 DSP |
| L1 DSP | `src-tauri/src/dsp/`（resampler / meter / ring） | 重采样、hop 切片、环形缓冲、播放时钟伺服、频谱 | 不碰设备 |
| L2 引擎 | `src-tauri/src/engine/`（stage / pipeline / registry / column / components） | 列与行、信号装配、会话生命周期 | 不做设备枚举 |
| L3 计划 | `src-tauri/src/plan.rs` | `SessionPlan`：配置 → 可启动描述；纯函数 | 不做 I/O |
| L4 推理 | `src-tauri/src/infer/` | ONNX 会话加载/卸载与流式推理 | 不碰设备 |
| L5 界面 | `src-tauri/ui/`、`src-tauri/src/main.rs` | 面板、命令、调试接口 | 不做信号处理 |

**单一实现路径**：DSP 只在 L1，播放缓冲策略只在 L1，设备枚举只在 L0，推理会话只在 L4。
新增音频功能 = 新增一个 Stage 或一种行/源，不改骨架。

---

## 2. 数据单位与不变量

- **内部格式恒为 F32 单声道 48 kHz**；原生采样率/声道只在 L0 转换（下混 + 重采样）。
- **10 ms hop 是唯一时间粒度**：`SAMPLE_RATE = 48_000`，`HOP = 480`，`NFFT = 960`，`FREQ = 481`。
  所有数据面按 hop 整数倍前进，禁止 1024/2048 等错位块。
- **一帧 = 480 样本**，链路内就地处理（`&mut [f32]`），数据面不新增分配。
- **设备回调是唯一主时钟**：回调只做「取样 / 补静音 / 写无锁环 + 原子计数」，
  **禁止在回调里写任何缓冲策略**。
- 采样率与 hop 唯一定义在 `audio/mod.rs`，其它模块一律引用。

---

## 3. 列（Column）与行（Row）

### 3.1 结构

**一个会话 = 若干列；一列 = 一条自上而下的有序节点列表；列与列完全独立（不混合）。**

```
列 1（默认，可增加更多列）
┌───────────────────────────────────────────────┐
│ 【输入】 mic（固定，不可删，下拉可换类型）        │  ← 最上面必须是输入
│   输入   loopback（可加，可拖动）                │
│   处理   denoise（可加，可拖动）                 │
│   输出   扬声器（可加，可拖动）                  │
│   处理   eq（可加，可拖动）                      │
│ 【输出】 CABLE Input（固定，不可删，下拉可换类型）│  ← 最下面必须是输出
└───────────────────────────────────────────────┘
```

- **最上面一行必须是输入、最下面一行必须是输出**，两者默认存在、**不可删除**，类型用下拉切换。
- **中间可任意增删、拖动**输入 / 输出 / 处理节点；增加的输入与输出也遵守「只能在最上面输入
  之下、最下面输出之上」。
- **列与列之间零耦合**：各自的输入、各自的处理、各自的输出。

### 3.2 一列的执行语义（自上而下累加）

```
acc = 0
for row in column.rows:          # 从最上面到最下面
    Input  → acc += 该源的一个 hop × k    # 多个输入在此混合
    Process→ acc = stage.process(acc)     # 就地处理
    Output → 把当前的 acc 送到该输出       # 位置 tap：取「到这里为止」的信号
    Viz    → 把当前的 acc 交给可视化缓冲   # 只读，不入信号路径
```

- 信号在一个 hop 内自上而下流动；**输出的位置决定它拿到哪一段信号**（放在处理之前即原始声）。
- 多个输入按 `k = 1 / max(1, 该列的输入数)`（用**配置的**输入数，避免拔插时增益跳变）混合。
- 一列至少是「输入 → 输出」（直通）。

### 3.3 列的运行方式

每列一个**列工作线程**，由系统时钟按 10 ms 驱动：

```
各输入行: 采集线程 → [L0 原生→48k, 切 hop] → 行输入环(有界)
列工作线程: 每 10ms:  从每个输入行的环各取 1 hop（空则补静音）→ 按 §3.2 执行整列
                     → 输出行把 acc 推给该输出的扇出环 → 输出设备(playback + PI 时钟伺服)
```

- **链上所有处理都在列工作线程里跑**（不在采集回调里），模型/滤波器状态天然**每列一份**。
- 每个输入行一个输入环（目标 2 hop，超 4 hop 丢最旧，空则补静音）；每个输出一个扇出环（1 s）。
- 设备与系统时钟的速率差由「输入环水位 + 输出 PI 伺服（±3%）」消化，列工作线程不需要知道任何设备时钟。
- **节拍必须绝对对齐 10 ms 网格**：`next += 10ms; sleep(next - now)`，**不能**用 `sleep(10ms - 本轮已用)`。
  后者每轮把 sleep 过冲累加，实测只产 ~96 hop/s；输出设备仍按 100 hop/s 消耗，4% 缺口超出 ±3% 伺服，
  会周期性抽干 48k 环 → 约 1 s 一次的欠载 + 重同步（听感为周期性断档）。节拍速率由列概要的
  `hop/s` 字段观测，正常必须稳定在 100。测试音等系统时钟源同理用绝对时刻节拍。

### 3.4 Stage 契约

```rust
pub struct FrameContext { pub ts: f64 }   // 当前无必需字段，固定签名

pub trait Stage: Send {
    fn name(&self) -> &'static str;
    fn accepts(&self, _ctx: &FrameContext) -> bool { true }
    fn process(&mut self, frame: &mut [f32], ctx: &FrameContext) -> Result<(), StageError>;
    fn reset(&mut self) {}
    fn release(&mut self) {}
}
pub enum StageError { Unavailable(String), Fatal(String) }
```

- 进出一律 480；组件不做采样率/声道转换、不切 hop。
- `Fatal` → 该列标记不可用并显示原因；`Unavailable` → 本帧跳过（直通）。
- 组件实例属于**行**（每列每行一份），因此流式状态按行隔离。

### 3.5 注册表

```rust
pub enum NodeKind { Input, Process, Output, Viz }
pub struct NodeSpec { pub ptype: &'static str, pub label: &'static str,
                      pub kind: NodeKind, pub params: &'static [ParamSpec] }
pub fn all_specs() -> &'static [NodeSpec];                       // 下拉与计划唯一来源
pub fn get_spec(ptype: &str) -> Option<&'static NodeSpec>;
pub fn create_stage(ptype: &str, params: &Params) -> Result<Box<dyn Stage>, String>;  // 仅 Process
```

- 输入 / 输出节点不是 Stage（它们是源与汇），由 L0 实现，注册表里只登记它们的类型与参数。

### 3.6 可视化

- viz 是**列内的一种行**，取它所在位置的信号（只读）。
- 缓冲有界：`cap = 48_000 * 5`（5 s），满时丢最旧。**禁止未上界的可视化缓冲**。

---

## 4. 节点清单

| 类别 | ptype（示例） | 说明 |
| --- | --- | --- |
| 输入 | `audio_input` / `loopback` / `remote_mic` / `echo_cancel` | 源；同列多个混合 |
| 处理 | `denoise_*` / `eq` / `gain` / `agc` / `compressor` / `soundpad` / `player` | Stage；就地处理 |
| 输出 | `audio_output` / `remote_speaker` | 汇；位置 tap，送往设备或网络 |
| 可视化 | `vu_meter` / `spectrum` | 只读 tap |

- **AEC 行**：一个输入行 =「一路 mic + 一路 far」，mic 在行内先过 AEC 再进列；
  far 参考只给模型用，**不进信号路径**；历史不足则直通 mic。
- **桌面声音（`loopback` 输入行）**：捕获某只输出设备正在播放的系统混音作为源
  （复用 AEC 的远端回环实现：Windows = WASAPI loopback，Linux = PipeWire monitor）。
  `device` = `loopback`（系统默认输出）或 `loopback:<端点ID｜sink 名>`；带行内增益
  `volume_db`（-30~6 dB，默认 -6 dB，系统混音偏响）。
- **媒体行（`soundpad` / `player`）是处理行，不是输入行**：无采集设备，在自身链位置
  把媒体样本与信号相加——挂链尾（默认）= 后级直通，媒体不经上游处理；拖到降噪之前
  则参与处理（与 legacy 链注入语义一致，也符合 §3.2 的位置 tap 语义）。
  - `soundpad`（音效板）：音效整段入内存（都是短音效，起播零延迟），起播/结束/
    手动停止/重播一律 hop 内线性淡变（杜绝咔哒，**首样本恒为 0**）；重播同一音效 = 旧声部
    淡出、新声部从**开头**起播；音量**每个音效一个**（±10 dB），拖动实时生效。
    每个音效可绑**一个全局热键**（录键 + 清除），热键**存在计划里**（`pads` 参数的
    `hotkey` 字段）——跟着音效走，增删音效即重注册，不另设一份设置。
  - `player`（音乐播放器）：流式解码（后台线程 → 有界环形缓冲 → 音频线程每 hop 取
    480 样本，内存恒定），曲目视作首尾相连的**环流**（EOF 回卷续播）；播放/暂停/seek/
    音量；位置经 `resume_sec` 持久化（暂停/拖动/退出即写）。
  - **媒体库**：导入时一律归一为 48 kHz 单声道 16-bit WAV，存 `~/.purevox/soundpad/`
    （音效）或 `~/.purevox/music/`（曲目）；文件名 `<原名>-<sha1 前 8 位>.wav`
    （同内容幂等复用）；配置只引用库内路径（不依赖外部原始文件）。
  - **控制面**：播放/停止/seek/音量是**运行时命令**（不是计划参数），经全局
    `MediaHub` 按 (列, 行) 寻址下发（与 `NetHub` 同款单例）；音量等热参数同时经
    `persist_plan` 落盘**不重建会话**；增删音效 / 换曲 = 结构性变更 → `apply_plan` 重建。
- **网络行**（本轮新增）：
  - `remote_mic` = 手机麦克风当电脑麦克风。**不需要虚拟声卡驱动**：网络流直接成为引擎的
    「源」，因此能进降噪 / TSE / AEC 全链。全会话只允许一个（全局单例）。
  - `remote_speaker` = 手机扬声器当电脑音箱。按 §3.2 位置 tap 取信号，允许多个（都收到同一份）；
    该信号经 Opus 编码后广播给所有已订阅客户端。
  - 协议、缓冲与安全边界见 §4.1。

### 4.1 网络协议（手机 ⇄ 电脑）

**传输**：WebSocket（明文 `ws://`，局域网），端口 **59123**（沿用旧实现），路径 `/ws`。
只绑局域网可达地址；这是**用户可见功能**，与 AGENTS §1.2 的本机调试接口（只绑 127.0.0.1）无关。

**编码**：Opus（`libopus`）。**不引入 C 编译链**：Rust 侧运行时 `LoadLibraryExW` +
`GetProcAddress` 加载预编译 x64 `opus.dll`（随包分发），Android 侧用系统自带的 `MediaCodec`
（`audio/opus`）。**帧长不固定**：PC 编码 10 ms（= 1 hop），Android 侧多为 20 ms；接收端按
**样本数累积**再重切成 480 样本 hop，因此 §2 的 10 ms 网格在引擎侧始终成立。

| 方向 | WebSocket 类型 | 内容 |
| --- | --- | --- |
| 手机 → 电脑 | Binary | 一个 Opus 包（解码出 N 个样本，按序累积切 hop） |
| 电脑 → 手机 | Binary | 同上（PC 侧只发 10 ms 包） |
| 双向 | Text | JSON 控制消息 |

控制消息（JSON，`t` 判别）：

| `t` | 方向 | 字段 | 含义 |
| --- | --- | --- | --- |
| `hello` | 客户端 → 服务端 | `proto` | 握手，`proto = 1` |
| `sub` | 客户端 → 服务端 | — | 订阅电脑音频（电脑当手机音箱） |
| `text` | 客户端 → 服务端 | `s` | 手机输入法提交的文本 |
| `key` | 客户端 → 服务端 | `code`、`down` | 手机实体按键（`code` = Android `KeyEvent.KEYCODE_*`） |
| `ping` / `pong` | 双向 | `id` | 往返时延测量 |
| `ready` | 服务端 → 客户端 | `codec`、`rate`、`channels`、`frame_ms`、`clients` | 握手成功 |
| `err` | 服务端 → 客户端 | `msg` | 原因（协议版本不符 / 远程输入未开启） |

**远程输入（`text` / `key`）**：

- **独立于音频**：走文本帧，不进信号路径。
- **两件事分开**：`text` = 手机输入法（已组合好的字符串，`KEYEVENTF_UNICODE` 注入，中文与
  emoji 都能打）；`key` = 手机实体按键当**全尺寸键盘**（Android keycode → Windows Set 1
  scancode 映射表，含扩展键标记，`SendInput` 注入）。
- **默认关闭**：须在界面显式开启「远程输入」才接受 `text` / `key`，否则回 `err`。
- **无鉴权（明确取舍）**：同网段可直接连。代价是局域网内任何人都能向本机注入按键；因此该
    开关默认关闭、界面常驻显示状态、界面上有一键关闭。

### 4.2 更新

**唯一数据源 = 本仓库 GitHub Releases**（`repos/a2heng/PureVox/releases/latest`）。
不引入更新插件、不自建 manifest：发版流水线（`.github/workflows/release.yml`）本来就把
产物挂到 release 上，两端靠**文件名后缀**认领（不看版本号——版本号只有年月日，同日会重复）。

| 阶段 | 含义 | 进入条件 |
| --- | --- | --- |
| `idle` | 尚未检查（开发构建也停在这，**不下「已是最新」的结论**） | 初始 / 开发构建 |
| `checking` → `up_to_date` / `available` | 查 GitHub 并比较 | 用户点「检查更新」 |
| `downloading` → `ready` | 下载本平台产物到 `~/.purevox/update/` | 状态 `available` |
| `installing` | 平台安装器已拉起，进程即将退出 | 状态 `ready` |
| `unsupported` / `failed` | 本平台不能自动装 / 出错（带原因） | 安装失败 |

- **比较依据 = tag 的完整时间戳**（`v2026.10.08.1430`），不是显示版本号 `26.10.8`
  ——同日发两次时显示版本相同，靠时分才能分出新旧。解析见 `version::parse_stamp`。
  开发构建没有 CI 注入的时间戳（`version::is_dev()`），此时只报「开发构建」，
  **绝不拿 Cargo 的 `0.1.0` 去和线上比出一个假结论**。
- **产物匹配只看后缀**：Windows `-setup.exe` → `.msi`；Linux `.AppImage` → `.deb` →
  `.rpm`；Android `.apk`。CI 统一重命名产物，后缀因此稳定。
- **安装是平台实现**（`update/install_<platform>.rs`）：Windows 用 NSIS 静默
  `/S /UPDATE /NS /R`，且**必须先退进程再装**（安装器会拦下正在运行的 exe）；
  Linux 仅 AppImage 能自替换；其余明确「不支持 + 给出 release 页面」，**不静默失败**。
- 状态同时进调试接口 `/debug` 的 `update` 段（与界面同源，AGENTS.md §1.2）。

---

## 5. SessionPlan（L3）

```rust
pub struct SessionPlan { pub columns: Vec<ColumnSpec>, pub problems: Vec<String>, pub warnings: Vec<String> }
pub struct ColumnSpec { pub rows: Vec<RowSpec> }        // rows[0] 必为 Input，rows[last] 必为 Output
pub struct RowSpec {
    pub kind: NodeKind,
    pub ptype: String,
    pub enabled: bool,
    pub params: Params,
    pub device: Option<DeviceRef>,   // 输入/输出行
    pub fixed: bool,                 // 两端的默认输入/输出 = true（不可删）
}
```

- `SessionPlan::from_config(cfg)`：**纯函数**，只读注册表与配置，不做 I/O。
- 校验：每列首行必须是输入、末行必须是输出（否则 `problems`）；网络行仅一个；无输出等。
- **变更语义**：结构性变更（增删/排序行、开关节点、换设备、换型号、增删列）→ **整会话重启**；
  热参数（增益、EQ 频点、阈值、媒体音量/seek）→ 下发给运行中的实例，不重启
  （媒体行经 `MediaHub` 实时命令 + `persist_plan` 落盘，见 §4）。

---

## 6. 设备生命周期

### 6.1 枚举与刷新（唯一入口）

- 枚举只用 cpal（`devices.rs`），**唯一入口** `devices::spawn_refresh`，后台线程执行。
- 触发点只有两个：**程序启动**、**面板上显眼的「刷新设备」按钮**。

### 6.2 标识与持久化

- **标识用 cpal 稳定 `DeviceId`**（不用名字；旧实现按名字模糊匹配是有意修正掉的坑）。
- 配置键按接口后缀写全：`input_device_<host>` / `output_device_<host>` / `aec_far_sink_<host>`，
  全部接口显式写全（占位保留）。
- 设备不存在时条目**保留**并标记不可用（附原因），**不静默替换**。

### 6.3 刷新对运行中链路的影响

**刷新只更新列表，不动正在跑的流，不重建链路。**

- 流按设备 ID 绑定：设备还在 → 不受影响；设备不在 → 该行标「设备不在（+原因）」并停止产出
  （回调停顿 ≥ 1 s 判定），界面给「重新打开」；**只标记、不动流**，不自动换设备、不自动重连。
- 只有**改设备选择**（结构性变更）才重启会话。

### 6.4 流的加载 / 卸载

- **两态 + 一个原因**：运行中 / 已停止 + 不可用原因；无状态机、无重试计数。
- 停止即停线程、关流、释放该行对模型的引用。
- 打开/关闭设备不在 UI 线程（异步命令 + 阻塞线程池）。

---

## 7. 模型生命周期

```rust
pub enum ModelKind { Denoise, Tse, Aec, TseRefEncoder }
pub struct ModelDesc { pub key: &'static str, pub file: &'static str, pub label: &'static str, pub kind: ModelKind }
pub fn models() -> &'static [ModelDesc];
pub fn resolve(key: &str, kind: ModelKind) -> Option<&'static ModelDesc>;
```

- 现存：`purevox_denoise_202609c_ep0012.onnx`（现役）、`202609b` / `202609a` / `202606`、
  `purevox_aec_202609_cpx_ep0375.onnx`、`purevox_tse_202609c_ep0201.onnx`（+ `..._ref_encoder.onnx`）。
  **缓存维度从模型输入读，不写死**。
- 选择存配置（`denoise_model` 等），计划构建时校验文件存在，缺失进 `problems`。
- 会话配置：`with_intra_threads(1)` + `with_inter_threads(1)` + `with_intra_op_spinning(false)`
  （实时音频必须；默认多线程忙等会占满 CPU 并把推理拖慢 50 倍）。
- **权重按文件共享一份**（`Arc<Session>`），**`cache` 每行一份**；惰性加载；行/会话停止即释放引用。
- **不做空闲自动卸载**；**不热切换模型文件**（改型号 = 结构性变更 → 重启）。

---

## 8. 内存管理

| 缓冲 | 位置 | 容量 / 策略 |
| --- | --- | --- |
| 采集回调 → 工作线程环 | `audio::capture`（rtrb） | 原生 500 ms；满则丢样并计数 |
| 行输入环 | `engine` | 目标 2 hop，超 4 hop 丢最旧；空补静音 |
| 输出扇出环 | `audio::fanout`（rtrb） | 1 s/订阅者；满则丢最旧 |
| 源缓冲（输出侧） | `audio::playback` | 目标 40 ms，封顶 300 ms，超限丢最旧 |
| 设备侧环 | `audio::playback` | 目标 30 ms |
| 重采样状态 | `dsp::resampler`（rubato） | 固定（数帧） |
| viz 缓冲 | `engine` | 5 s，丢最旧 |
| 频谱 / 波形 | `dsp::meter` | 波形 50 ms、频谱 481 bin，固定 |
| AEC far 历史 | `engine` | 2 s 网格；`far_delay` ≤ 1000 ms |
| 回环历史 | `engine` | 1.5 s |
| 音效板声部 | `engine` | 每个音效一段 48k/mono 样本（短音效整段入内存） |
| 播放器环形缓冲 | `engine` | 3 s（48k f32），解码线程写、音频线程读，超限丢最旧 |
| 网络 acc | `net/remote_mic` / `net/remote_speaker` | 进出各一环，目标 50 ms，硬顶 80 ms（超限丢最旧）；取不足一个 hop 补静音 |
| 模型 `cache` | `infer` | 由模型决定（降噪 36506 / AEC 215504 / TSE 513216 f32），**每行一份** |

规则：**音频回调零分配零加锁零日志**；工作线程预分配复用；任何进入数据面的容器都必须有上限与丢弃策略。

---

## 9. 扩展指南

- **新增处理组件**：`engine/components/` 新建实现 `Stage` → 注册表登记 `NodeSpec{kind: Process}` →
  冒烟测试 → 用户可感知的变更追加 `about/changelog.md`。
- **新增输入/输出**：注册表登记 + 实现 L0 端点 + `SessionPlan` 提取规则 + 单测。
- 必须遵守：先扩展再新建；被替代实现直接删除、不留平行；DSP 只在 L1；不改骨架。

---

## 10. 现状对照与实施顺序

| 能力 | 现状 | 目标 |
| --- | --- | --- |
| 设备枚举（cpal / 稳定 ID）+ 刷新按钮 | ✅ 已实现（顶栏「刷新设备」，只更新列表、不动流） | §6 |
| 采集 → 重采样 → 48k hop | ✅ 已实现 | 作为「输入行」的源 |
| 输出 + 时钟伺服 | ✅ 已实现 | 作为「输出行」的汇 |
| 降噪 ONNX | ✅ 已实现（1.8 ms/hop） | 包成 `Stage`，放进列 |
| 调试面板 + HTTP 调试接口 | ✅ 已实现（含 `ui` 前端自报、`column` 列概要） | 补行状态、模型字段 |
| **Stage / Pipeline / 注册表** | ✅ 已实现 | §3.4 / 3.5 |
| **列工作线程 + 行环** | ✅ 已实现（多列独立、自上而下位置语义） | §3.3 |
| **配置 + SessionPlan** | ◑ 列/行/设备/型号已接入并持久化；热参数下发未做 | §5 |
| **列 UI（增删列、加/拖行、两端固定）** | ✅ 已实现（增删列、加/删/上下移行、两端固定；拖动待做） | §3.1 |
| **模型注册表 + 型号选择** | ◑ 降噪/TSE 型号下拉已接入；统一 `ModelDesc` 注册表待做 | §7 |
| **TSE 目标说话人提取** | ✅ 已实现（参考 10 s → `enr_tok`，3.9 ms/hop；参考可录制=降噪后+音量归一化；无参考直通） | §4 / §7 |
| **AEC 回声消除** | ✅ 已实现（far 参考采集 + 2 s 网格对齐 + 每行缓存 + 互相关延时校准） | §4 |
| 回环 | ✅ 已实现（`audio/loopback.rs`，WASAPI loopback） | 作为「输入行」的源（`loopback`，电脑播放音频） |
| 媒体 | ✅ 已实现（后端 + 界面，2026-10-12）：`soundpad` / `player` 处理行、媒体库导入（symphonia）、每音效热键、MediaHub 控制 | §4 |
| **网络（手机 ⇄ 电脑）** | ✅ 已实现：`remote_mic` 输入行 + `remote_speaker` 输出行 + Opus（运行时加载预编译 `opus.dll`）+ `text` 打字 / `key` 全尺寸键盘（默认关闭，界面开关） | §4 / §4.1 |
| **更新检查 / 更新** | ✅ 已实现（2026-10-12）：GitHub Releases 查 → 下载 → 静默安装（Windows NSIS / Linux AppImage 自替换），顶栏「更新」面板 | §4.2 |
| **版本号 年.月.日** | ✅ 已实现（2026-10-12）：`yy.m.d` 显示、tag 全时间戳比较；Android `versionCode` = `yyMMddHHmm` 单调递增 | — |

**实施顺序**：
1. ✅ Stage / Pipeline / 注册表骨架 + 现有降噪包成 Stage。
2. ✅ 列工作线程 + 行输入环（源 → 列 → 输出；多列独立）。
3. ◑ 配置 + SessionPlan（计划/校验/持久化/重建已接入；热参数待做）。
4. ◑ 列 UI（两端固定、中间加/删/移、增删列已接入；拖动排序待做）。
5. 刷新按钮 + 行状态展示（含「重新打开」）。
6. ◑ 模型注册表 + 型号选择（降噪/TSE/AEC 已接入）。
7. ✅ TSE、✅ AEC（含参考录制、延时校准）；✅ 回环；✅ 网络（§4.1）；→ 媒体（音板 / 系统声）。
