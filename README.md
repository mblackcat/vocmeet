# VocMeet（声会）

离线会议纪要系统。全量录制 → 会后本地转写 → 说话人分离 → 结合速记生成纪要。
原始音频与逐字稿默认不出本机。

方案文档：[`docs/vocmeet_mvp_plan.md`](docs/vocmeet_mvp_plan.md)（详细设计、验收指标、实测数据、与方案的偏差都记在这里）

---

## 当前实现状态

MVP 分三层交付，各层验证程度不同——下表是实事求是的状态，不是计划：

| 组件 | 状态 | 验证方式 |
| :--- | :--- | :--- |
| `crates/core` 核心库 | ✅ 可用 | 94 个单元测试通过 |
| `crates/capture` 音频采集 | ✅ 可用（Windows / macOS 14.6+） | 8 个单元测试；Windows 实录双轨 8 秒、macOS 实录双轨 12 秒均通过 |
| `crates/cli` 命令行 | ✅ 可用 | 全链路手工验证，详见方案文档 §14.3 |
| `src-tauri` + React 桌面 GUI | ⚠️ 可启动，交互未验证 | 编译通过、进程实启存活、安装包已产出；**UI 未做人工点击验证** |

GUI 已实现并能打出安装包（Windows：MSI / NSIS；macOS：app / dmg，见下方「桌面应用」）。
但本机环境无法做人机交互，「界面能打开、前端资源加载正常、后端命令注册无误」是验证过的，
**「每个按钮点下去行为正确」没有验证过**。首次使用请按下方步骤逐项手工确认。

---

## 桌面应用

安装包在 `src-tauri/target/release/bundle/` 下：

- `msi/VocMeet_0.1.0_x64_en-US.msi`（13.9 MB）
- `nsis/VocMeet_0.1.0_x64-setup.exe`（9.8 MB）
- macOS：`macos/VocMeet.app` 与 `dmg/VocMeet_0.1.0_aarch64.dmg`

安装包**不含 ONNX 权重**（约 360MB）。首次启动到设置页点「一键下载模型」即可，
下完自动生效，不用手动指目录。没有网络或者已经有一套权重的话，
也可以跑 `scripts/fetch-models.sh`，再用「已经有了，指个目录」指过去。

**macOS 采集须知**：系统音频采集走 CoreAudio process tap（经 cpal），**需要 macOS 14.6 或更高版本**；
低于该版本时「录制」页会明确报错，导入音频、逐字稿、纪要等其余功能不受影响。

首次录制前请确认两项权限，否则会静默录出空音频：

| 权限 | 位置 | 不给的后果 |
| :--- | :--- | :--- |
| 麦克风 | 系统设置 → 隐私与安全性 → 麦克风 | 麦克风轨为空 |
| 系统录音 | 系统设置 → 隐私与安全性 → 系统录音 | **系统轨静默录成空音频，不报错也不弹窗** |

「系统录音」这条是 macOS 的一个坑：权限没给时 CoreAudio 照样把流开起来，只是录到的全是静音。
VocMeet 会在录制结束后检查系统轨，没采到声音时给出明确警告并指向上面这个设置项；
自检页的「系统回环」一行也会如实区分「打不开」和「能打开但没采到声音」。

还有一条只在**首次**录制时出现的现象：第一次访问麦克风要过系统的权限检查，
那一次麦克风轨会比系统轨短 3 秒左右（实测 8.570s vs 11.748s）。之后每次录制都稳定在 25ms 以内。
重要的会议建议先空录几秒，把这一次性开销跑掉。

安装包未签名公证，首次打开需要 `xattr -dr com.apple.quarantine /Applications/VocMeet.app` 放行。

界面分五页：**录制**（开始/停止、会中速记 Scratchpad、发起转写并看进度）、
**逐字稿**（说话人命名、低置信片段高亮、就地校对、导出）、
**纪要**（流式生成、速记与逐字稿一起送模型）、
**设置**（出网策略、LLM 端点、API Key 存凭据管理器、聚类阈值）、
**自检**（模型校验和、数据库与凭据管理器、音频设备与系统回环）。

手上已经有录音的话，**把音频文件拖进窗口**就能直接变成一场会议，不必先录一遍，详见下方
「导入已有音频」。

开发模式：

```bash
npm install
npx tauri dev
```

首次手工验证建议顺序：自检页全绿 → 录一段 30 秒 → 转写（填参会人数）→ 命名说话人 → 导出。

### 首次配置

设置页底部「这台机器」一段会如实报当前状态，两样东西各有一个一键入口：

| 缺什么 | 怎么办 |
| :--- | :--- |
| 识别模型（ONNX 权重，约 265MB 下载 / 359MB 解压） | 点「一键下载模型」。下载 → 解压 → 校验 SHA256 → 写回配置，全自动 |
| 写纪要的模型（本机 Ollama） | 端点体检会分清「服务没起来」「起来了但没装模型」「模型名不对」，缺模型时直接给「拉取 qwen2.5:7b」这类按钮 |

模型输入框带下拉，列的是端点上**真实装着**的模型——打错名字只会得到一个 404，
而 404 看起来跟「服务没起来」一模一样，很难排查。

权重存在 `%LOCALAPPDATA%\VocMeet\models`（macOS/Linux 回落到 `~/.vocmeet/models`），配置存在同目录的 `vocmeet.config.json`。

### 重新整理一场会议

会议页底部有两个按钮：

- **重写纪要** —— 逐字稿不动，只用当前配置的模型重写一遍。对这版不满意、或者换了个更大的模型时用。
- **重新解析录音** —— 从盘上的录音重跑：识别 → 说话人分离 → 纪要。改了聚类阈值或者识别线程之后用。

两个都按**按下去那一刻**的配置执行。旧纪要不会被删（`summaries` 是追加表），
界面显示最新的一版。录制的会议优先用双轨分片重跑（麦克风轨不做分离，见 §4.1），
分片不在了才退回混音后的 `playback.wav`；导入的会议本来就只有一条轨。

---

## 导入已有音频

把文件拖进窗口任意位置，或在录制页点「选个文件」。松手之后不需要再点任何按钮：
解码 → 转写 → 说话人分离 → 纪要，与录制结束后的流程完全一致。

支持的容器与编码由 [symphonia](https://github.com/pdeljanov/Symphonia) 提供，纯 Rust，不依赖 ffmpeg：

```
wav / wave   aac   m4a / m4b / mp4   mp3   flac
ogg / oga    mka / webm   aiff / aif / aifc   caf
```

几个值得知道的行为：

- **整条音轨按「系统轨」处理**，也就是会做说话人分离。导入的录音里所有人都在同一条轨上，
  双轨录制里「麦克风轨 = 本机用户」那条免分离的捷径用不上（§4.1）。
- **解码是流式的**：边解边写 16kHz 单声道 WAV，不把整段 PCM 攒在内存里，一小时的音频也不会撑爆内存。
- **落盘的永远是 WAV**，转写与回放共用同一个文件——顺带也绕开了「webview 未必认识 .aac」这件事。
- **单个坏包会被跳过**而不是让整场导入失败，跳过数量记在日志里。
- 转写是单任务串行的（§6），所以一次只处理一个文件；同时拖多个只会开始第一个。
- **长会议（一小时以上）务必填参会人数**：聚类阈值在长音频上容易把同一个人切成多个说话人标签，
  填了人数走 `num_clusters` 会准得多。实测数据见方案 §14.3。

---

## 快速开始（CLI）

### 依赖

- Windows 10/11，或 macOS 14.6+（采集层支持这两个平台）
- Rust stable（Windows 上需 MSVC toolchain）
- 约 600MB 磁盘（ONNX 权重）

### 构建

```bash
bash scripts/fetch-models.sh models     # 下载 ONNX 权重（约 360MB 解压后）
cargo build --release -p vocmeet-cli
```

### 自检

```bash
./target/release/vocmeet-cli --models models doctor
```

会校验六个模型文件的存在与 SHA256，并确认 SQLite 与 Windows Credential Manager 可用。

CLI 与 GUI 共用同一套 core，数据互通（同一个数据库）。CLI 同时是 W1 bake-off 的评测载体。

### 全链路

```bash
B=./target/release/vocmeet-cli

# 1. 看看有哪些音频设备，确认系统回环可用
$B devices

# 2. 录制双轨（麦克风 + 系统回环），60 秒一个分片
$B record --seconds 600 --out recordings

# 3. 转写。已知参会人数时务必传 --speakers，准确率明显更高
$B --models models transcribe \
   --system recordings/sys_00000.wav \
   --mic    recordings/mic_00000.wav \
   --speakers 4 --title "季度评审"

# 4. 给说话人命名（不带参数会列出待命名的 spk_N 及其发言样例）
$B name 1
$B name 1 spk_0=张三 spk_1=李四

# 5. 写速记笔记
$B note 1 "重点：确认预算与排期"

# 6. 生成纪要（默认只允许本机 LLM 端点）
$B summarize 1

# 7. 导出
$B export 1 --out 季度评审.md

# 8. 查看出网审计
$B audit
```

### 纪要生成的前置条件

默认配置指向本地 Ollama：

```bash
ollama serve
ollama pull qwen2.5:7b
```

要用云端模型，需要在配置里把 `egress_policy` 改成 `open` 并设置 `llm.api_base`。
**这会让逐字稿离开本机**——`local_only` 策略下任何非本机端点都会在请求发出前被拦截。

---

## 实测数据与和方案的偏差

详细实测记录（RTF、双轨对齐误差、说话人聚类准确率等）与设计偏差（SQLCipher → SQLite 字段级
加密、rubato 版本变更等）记在方案文档 §14，不在此重复。**唯一需要提前知道的安全性降级**：整库加密改成了字段级 AES-256-GCM 加密（表结构、
时间戳、会议标题等元数据不加密），MVP 内部试用可接受，v1.0 前必须改回整库加密——
详见 `crates/core/src/store.rs` 顶部注释。

---

## 仓库结构

```
vocmeet/
├─ crates/
│  ├─ core/         核心库：audio / asr / align / store / llm / summarize / policy / jobs
│  ├─ capture/      双轨采集：Windows WASAPI / macOS CoreAudio tap
│  └─ cli/          命令行入口
├─ src-tauri/       Tauri 后端：命令层与事件推送
├─ src/             React 前端：录制 / 逐字稿 / 纪要 / 设置 / 自检
├─ models/          ONNX 权重（不入库，由 scripts/fetch-models.sh 下载）
├─ templates/       提示词模板（YAML + minijinja）
├─ scripts/         模型下载
└─ docs/            方案文档
```

核心模块与方案章节一一对应，见 `crates/core/src/lib.rs` 顶部。

---

## 许可证注意

音频解码用的 `symphonia` 系列 crate 是 **MPL-2.0**：文件级弱著佐权，
静态链接进 Apache-2.0 应用不构成传染，改动 symphonia 自身的源码才需要回馈。
本仓库只调用不修改，无额外义务。

`models/` 下的权重各有其协议，**其中 SenseVoice 与 CT-Punc 适用阿里 FunASR 模型开源协议**，
该协议含「仅供参考和学习使用」表述。内部试用不构成阻断，**商业化前必须取得法务结论**。
pyannote-segmentation-3.0 为 MIT，3D-Speaker 为 Apache-2.0，均可商用。

详见方案 §8.2 与 §10.2。
