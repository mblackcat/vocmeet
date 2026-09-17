# macOS 音频采集设计（双轨，cpal + CoreAudio process tap）

日期：2026-09-11
状态：已确认，待实现
对应方案章节：§4.1（双轨采集）、§9.1（macOS 支持）

## 背景

`crates/capture` 目前只有 Windows（WASAPI）实现，非 Windows 平台是占位 fallback，
调用即返回 `CaptureError::Unsupported`。macOS 上「录制」页因此完全不可用，
但导入音频、转写、纪要、设置、自检等不依赖实时采集的链路是通的。

本设计把 macOS 提升到与 Windows 一致的**双轨采集**能力：麦克风轨与系统输出轨
分开录制、不在采集端混音，从而保留 §4.1 的核心设计——麦克风轨天然等于「本机用户」，
无需做说话人分离，准确率与速度都优于混轨后再聚类。

## 已核实的技术事实

以下均已联网核实（2026-09-11），**未核实项单列在最后一节，评审中不得当作已知事实**。

| 断言 | 结论 | 依据 |
| :--- | :--- | :--- |
| cpal 支持 macOS 系统回环录制 | 真。0.17.0（2025-12-20）新增：「CoreAudio: Support for loopback recording (recording system audio output) on macOS > 14.6」 | cpal CHANGELOG |
| cpal 最新发布版 | 0.18.2（2026-08-16） | crates.io API |
| 0.18.0 修了三个 loopback 缺陷 | 真。创建期未定义行为与静默失败、并发/崩溃后聚合设备 UID 冲突、tap 未自动启动导致录到静音 | cpal CHANGELOG 0.18.0 |
| 实现机制 | `AudioHardwareCreateProcessTap` + `CATapDescription` + `AudioHardwareCreateAggregateDevice`，经 `objc2-core-audio` | `src/host/coreaudio/macos/loopback.rs` |
| 调用方式 | 对**输出设备**调 `build_input_stream`；cpal 检测到 `!supports_input()` 后自动建 tap 与聚合设备 | `src/host/coreaudio/macos/device.rs` L811-L827 |
| **系统录音权限未授予时会静默录成静音** | 真。且 cpal **没有**任何检测助手：PR #1124 已关闭未合入，PR #1257 仍开启未合入 | GitHub API 查 PR merged=false |
| Tauri `bundle.macOS.infoPlist` 是**文件路径**而非内联对象 | 真。schema 类型为 `['string','null']`，描述为 "Path to a Info.plist file to merge with the default Info.plist" | schema.tauri.app/config/2 |
| `minimumSystemVersion` 默认 10.x | 真 | 同上 |

**权限这条是本设计最大的风险点**：CoreAudio 在缺少「系统录音」TCC 权限
（`kTCCServiceAudioCapture`）时，tap 会创建成功并录出全静音，不报错、不崩溃。
录完一场会才发现是空的代价极高，必须主动防御。

## 决策

1. **双轨对齐**，与 Windows 行为一致，后续链路（重新解析录音等）无需写平台分支。
2. **系统轨走 cpal，仅支持 macOS 14.6+**。不引入 ScreenCaptureKit——它虽然能覆盖
   macOS 13+，但需要「屏幕录制」权限，对一个主打「离线、不出本机」的会议工具来说，
   向用户索要屏幕录制权限的观感代价过高；且麦克风轨仍需 cpal，会变成两套 API 两条代码路径。
3. **静音探测 + 警告**，全部走公开 API，不依赖 Apple 私有 TCC 框架
   （cpal 自己都还没敢合入那个 PR）。

## 架构

### 模块布局

```rust
#[cfg(windows)]                                mod windows_impl  { … }  // 不动
#[cfg(target_os = "macos")]                    mod macos_impl    { … }  // 新增
#[cfg(not(any(windows, target_os = "macos")))] mod fallback_impl { … }  // 收窄 cfg 条件
```

Linux 保留 fallback，CI 的 ubuntu leg 才能继续编过。

依赖：

```toml
[target."cfg(target_os = \"macos\")".dependencies]
cpal = "0.18"
```

### 设备选择

两条轨都由 cpal 提供，与 Windows 的概念映射一一对应：

| 轨 | Windows（WASAPI） | macOS（cpal） |
| :--- | :--- | :--- |
| 麦克风 | 默认 Capture 设备 | `default_input_device()` |
| 系统输出 | 默认 **Render** 设备，以 Capture 方向初始化 | 默认 **output** 设备，调 `build_input_stream()` |

### 采集循环

WASAPI 是阻塞读，cpal 是回调驱动，因此循环结构不同：

```
stream = device.build_input_stream(cfg, data_cb, err_cb)
data_cb  → 把 f32 交错样本送进 mpsc（回调内不做重采样、不阻塞、不打日志）
主循环    → recv_timeout 取出 → downmix → resample_to_target → writer.push → 更新 peak
停止      → drop(stream) → 排空 channel → writer.finish()
```

复用 `vocmeet_core::audio::{downmix, resample_to_target}`，与 Windows 的
`decode_and_downsample` 走同一条转换路径；把共用尾段提成一个模块级 helper，
避免两个平台各写一份。

### 静音探测

`capture_track` 的返回类型加宽，带上峰值幅度：

```rust
pub struct TrackOutcome {
    pub chunks: Vec<ChunkInfo>,
    pub peak: f32,
}
```

`record_dual_track`（本就与平台无关）在 join 之后检查系统轨：若 `peak` 近似为 0
而分片非空，则写入**已有的** `Recording.warnings` 字段——该字段 UI 已在消费，
无需改前端：

> 系统音频全程静音。若当时确实有人说话，请检查「系统设置 → 隐私与安全性 → 系统录音」
> 是否已勾选 VocMeet（未授权时 macOS 会静默录成空音频，不报错）。

维护一个 running max 的开销可忽略，且这条防御对 Windows 同样有效——输出设备被
其他程序独占时是同一类静默失败。

### 自检页

`probe_loopback() -> Result<()>` 改为 `Result<String>`，让 Ok 分支也能带回说明。
`src-tauri/src/main.rs` 从 `Ok(()) => (true, "系统回环可用".to_string())`
改成 `Ok(msg) => (true, msg)`，**一行改动，不动类型定义、不动前端**。

这样做的原因：探测期间恰好没有声音在播放是完全正常的，不该因此把自检行标红；
但也必须把「可能未授权」这个提示传达出去。三态用一个 message 承载即可。

macOS 的 `probe_loopback` 行为：
- 建流失败 → `Err`，消息里带原因（14.6 以下会在这里失败）
- 建流成功但 400ms 内峰值为 0 → `Ok`，消息为「系统回环可打开，但探测 400ms 未采到声音；
  若当时有音频在播放，请检查系统录音权限」
- 正常 → `Ok("系统回环可用")`

### 打包配置

新增 `src-tauri/Info.plist`，含 `NSMicrophoneUsageDescription`（缺失会被 macOS 直接拒绝麦克风访问）：

```xml
<key>NSMicrophoneUsageDescription</key>
<string>VocMeet 需要访问麦克风以录制您本人在会议中的发言。音频仅保存在本机。</string>
```

`tauri.conf.json` 增加 `"macOS": { "infoPlist": "Info.plist" }`。

**不设 `minimumSystemVersion`**：录制需要 14.6+，但导入音频、转写、纪要在更老的
macOS 上是可用的，把整个应用锁死在 14.6 以上是净损失。老系统上录制会得到明确报错，
而不是崩溃或静默失败。

## 测试策略

- **单元测试**只覆盖纯逻辑：峰值跟踪、静音判定阈值、警告文案生成。
  真实音频采集无法在 CI 里测。
- **CI**：`macos-latest` 的 `cargo check --workspace --tests` + `cargo test --workspace`
  捕获编译错误与纯逻辑回归。
- **真机验证**（必须，且只能在开发者本机做）：
  1. `vocmeet-cli devices` 列出设备
  2. `vocmeet-cli record --seconds 10`，期间播放音频并说话
  3. 确认 `mic_00000.wav` 与 `sys_00000.wav` **两条轨都有信号**（不是全静音）
  4. 双轨时长差 <200ms（§8.1 出口标准）
  5. 走完 transcribe → 确认麦克风轨未被送进 diarization

验证边界要如实记录，不得把「编译通过」写成「可用」——这是仓库既有的文档纪律。

## 未核实 / 仍属假设

1. **对输出设备调 `build_input_stream` 时该传什么 config**——本设计最大的实现期未知项。
   可能需要 `supported_input_configs()` 查聚合设备。实现时实测确定。
2. **未签名应用能否拿到系统录音权限**——TCC 弹窗可能根本不出现，需要用户手动在
   系统设置里添加 VocMeet。
3. **`tauri dev` 与打包后的 `.app` 是不同二进制**，权限授予可能不互通。
4. ~~**是否存在对应的 Info.plist key 来声明系统音频采集意图**~~——**已核实：存在，且是必需的。**
   key 是 `NSAudioCaptureUsageDescription`。不声明它，应用**根本不会出现在**
   「系统设置 → 隐私与安全性 → 屏幕与系统音频录制」的列表里，用户想授权也没得勾，
   而 tap 仍会创建成功并静默录出空音频——正是本文档开头那个坑的完整成因。
   本机取证：Granola / ChatGPT / Lark / VS Code 四个应用都声明了这个 key，
   其中 Granola（同类会议转写应用）出现在该页的 **"System Audio Recording Only"** 分区。
   已于 `src-tauri/Info.plist` 补上。
5. cpal 的 loopback 能力较新（2025-12 落地，2026-06 修缺陷），长时间录制稳定性未知。
6. **ad-hoc 签名与 TCC/Keychain 的稳定性**——实测每次重新构建后，`.app` 的 ad-hoc
   签名（cdhash）都会变，macOS 因此当成另一个应用：Keychain 会重新弹「VocMeet wants to
   use your confidential information」要求输入登录密码，TCC 授权同理需要重新授予。
   这不只是开发期的麻烦，**用户每次升级版本都会撞上**。根治要 Developer ID 证书
   （TCC 按 team id + bundle id 认应用，跨版本稳定）。

## 明确不做（YAGNI）

- 不引入 ScreenCaptureKit（理由见「决策」第 2 条）
- 不引导安装 BlackHole 虚拟声卡
- 不调用 Apple 私有 TCC 框架
- 不为 13–14.5 版本做降级路径；这些系统上录制直接报错，其余功能照常
- 不做采集后端 trait 抽象——当前只有一个实现，等真有第二个再抽
