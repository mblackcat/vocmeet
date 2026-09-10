# VocMeet（声会）

离线会议纪要系统。全量录制 → 会后本地转写 → 说话人分离 → 结合速记生成纪要。
原始音频与逐字稿默认不出本机。

方案文档：[`docs/vocmeet_mvp_plan.md`](docs/vocmeet_mvp_plan.md)

---

## 当前实现状态

MVP 分三层交付，各层验证程度不同——下表是实事求是的状态，不是计划：

| 组件 | 状态 | 验证方式 |
| :--- | :--- | :--- |
| `crates/core` 核心库 | ✅ 可用 | 94 个单元测试通过 |
| `crates/capture` 音频采集 | ✅ 可用（Windows） | 4 个单元测试 + 本机实录双轨 8 秒通过 |
| `crates/cli` 命令行 | ✅ 可用 | 全链路手工验证（见下方实测记录） |
| `src-tauri` + React 桌面 GUI | ⚠️ 可启动，交互未验证 | 编译通过、进程实启存活、安装包已产出；**UI 未做人工点击验证** |

GUI 已实现并能打出安装包（MSI / NSIS）。但需要说清楚验证边界：本机环境无法做人机交互，
所以「界面能打开、前端资源加载正常、后端命令注册无误」是验证过的，
**「每个按钮点下去行为正确」没有验证过**。首次使用请按下方步骤逐项手工确认。

---

## 桌面应用

安装包在 `src-tauri/target/release/bundle/` 下：

- `msi/VocMeet_0.1.0_x64_en-US.msi`（13.9 MB）
- `nsis/VocMeet_0.1.0_x64-setup.exe`（9.8 MB）

安装包**不含 ONNX 权重**（约 360MB）。首次启动到设置页点「一键下载模型」即可，
下完自动生效，不用手动指目录。没有网络或者已经有一套权重的话，
也可以跑 `scripts/fetch-models.sh`，再用「已经有了，指个目录」指过去。

界面分五页：**录制**（开始/停止、会中速记 Scratchpad、发起转写并看进度）、
**逐字稿**（说话人命名、低置信片段高亮、就地校对、导出）、
**纪要**（流式生成、速记与逐字稿一起送模型）、
**设置**（出网策略、LLM 端点、API Key 存凭据管理器、聚类阈值）、
**自检**（模型校验和、数据库与凭据管理器、音频设备与系统回环）。

手上已经有录音的话，**把音频文件拖进窗口**就能直接变成一场会议，
不必先录一遍。详见下方「导入已有音频」。

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

权重存在 `%LOCALAPPDATA%\VocMeet\models`，配置存在同目录的 `vocmeet.config.json`。

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
- **解码是流式的**：边解边写 16kHz 单声道 WAV，不把整段 PCM 攒在内存里。
  一小时 48k 立体声解出来是 GB 级，攒不得。
- **落盘的永远是 WAV**，转写与回放共用同一个文件——顺带也绕开了「webview 未必认识 .aac」这件事。
- **单个坏包会被跳过**而不是让整场导入失败，跳过数量记在日志里。
- 转写是单任务串行的（§6），所以一次只处理一个文件；同时拖多个只会开始第一个。

实测（本机，一段 41MB 的 `.aac`，16kHz 单声道 AAC-LC）：

```
85 分 33 秒的音频，解码 + 落盘耗时 1.03s，产出 164MB 的 16k 单声道 WAV
```

裸 ADTS（`.aac`）容器里没有时长字段的话，进度条会退化成「已解码 X」的计数——
这条链路上的这个文件是有的，所以百分比正常。

---

## 快速开始（CLI）

### 依赖

- Windows 10/11（采集层仅支持 Windows；macOS 见方案 §9）
- Rust stable（MSVC toolchain）
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

## 实测记录（本机，2026-08-27）

在 sherpa-onnx 官方的四人中文测试音频（56.9 秒）上：

```
音频 56.9s，耗时 5.5s，RTF = 0.096
```

- **RTF 0.096**，优于方案 §7 的 ≤0.15 目标（该机器为 x86 CPU，`num_threads=2`）
- 传 `--speakers 4` 时得到恰好 4 个说话人（spk_0…spk_3）
- 不传 `--speakers` 时聚类切出 6 个标签——**阈值默认值 0.5 偏低，会过分裂**，
  这正是方案 §7「说话人数估计准确率」指标要标定的参数
- 低置信片段 2/6 条，均为说话人边界处的跨段发言，符合预期行为

双轨录制实测：两轨时长 6.98s / 7.054s，差 74ms，优于 §8.1 的 <200ms 对齐目标。

### 长会议实测（85 分 33 秒，导入的 .aac，单轨）

```
解码 + 落盘 1.0s   →   识别 + 分离 + 标点 约 20 分钟（num_threads=2）
387 条发言，中文识别质量可用
未传参会人数 → 切出 217 个说话人标签
```

**217 个标签是不能用的。** 四人音频那次是「4 人切成 6 个」，长会议把同一个毛病放大到了
两个数量级——聚类阈值 0.5 在长音频上会把同一个人在不同段落切成不同的人。
在阈值标定完成之前，**长会议必须填参会人数**（会议页「重新解析录音」旁边就有这个输入框）。

另外，约 20 分钟的耗时远差于 RTF 0.096 的推算值（8 分钟）：瓶颈在 diarization，
它的开销随片段数超线性增长，56.9 秒样本上量不出来。这条也需要 W1 单独测。

> 这些数字来自单台机器的单个样本，**不能替代方案 §7 要求的 Set-A 测试集评测**。
> CER / DER 仍然是空白——需要人工转写的 golden 数据才能计算。

---

## 与方案的偏差

### SQLCipher → SQLite + 字段级 AEAD

方案 §3 选型表写的是 SQLCipher 整库加密。`rusqlite` 的
`bundled-sqlcipher-vendored-openssl` feature 需要从源码编译 OpenSSL，
在本机 Windows 工具链上因 Perl 模块缺失（`Locale::Maketext::Simple`）无法构建。

实现改为 **SQLite + 对敏感列做 AES-256-GCM 加密**，密钥仍托管在 Windows Credential Manager。

- 仍然加密：逐字稿正文、速记笔记、纪要正文
- 不再加密：表结构、时间戳、说话人 key、会议标题等元数据

对 MVP 内部试用可接受，但这是**安全性的实质降级**，v1.0 必须改回整库加密
（补 OpenSSL 构建环境，或改用 SQLCipher 预编译库）。详见 `crates/core/src/store.rs` 顶部注释。

### rubato 5.0 → 0.16

rubato 5.0 换成了基于 `audioadapter` 的新 API，`SincFixedIn` 已移除。
锁定 0.16.x（方案 §3 表格里的 5.0.0 是核实当时的最新版，但不适用）。

---

## 仓库结构

```
vocmeet/
├─ crates/
│  ├─ core/         核心库：audio / asr / align / store / llm / summarize / policy / jobs
│  ├─ capture/      Windows WASAPI 双轨采集
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
