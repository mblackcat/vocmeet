# VocMeet（声会）— 离线会议纪要系统 MVP 落地方案

| 项 | 值 |
| :--- | :--- |
| 文档版本 | MVP 1.0 |
| 日期 | 2026-08-27 |
| 状态 | 待评审 |
| 取代 | `vocmeet_implementation_plan.md`（v1.0）、`vocmeet_implementation_plan.v2.md`（v2.1），二者已删除 |
| 事实核实 | 库能力、API 形态、许可证均已联网核实，见 §10；未核实项在 §10.3 单独列出 |

---

## 1. MVP 定义

### 1.1 这一版要验证什么

VocMeet 的完整产品主张是「高隐私 + 高转写准确率 + 深度纪要提炼」的企业级离线会议系统。MVP 不去证明这三条全部成立，只回答一个问题：

> **在真实的中文会议上，「全量录制 → 会后离线转写 → 说话人分离 → 结合速记生成纪要」这条链路，产出的纪要是否好到让人愿意天天用？**

这个问题回答不了「是」，后面所有企业化功能（多平台、声纹库、合规策略、模板体系）都没有意义。所以 MVP 把工程复杂度压到最低，把**验证密度**拉到最高：单平台、单模板、单任务、内部试用。

### 1.2 交付形态

Windows 桌面应用，团队内部 dogfood 分发（不签名、不上架、不对外销售）。目标是连续 4 周、每周至少 10 场真实会议的使用数据。

### 1.3 范围

**MVP 做**

| 能力 | 说明 |
| :--- | :--- |
| Windows 双轨录制 | 麦克风 + 系统回环分轨落盘，60s 分片，崩溃/断电可恢复 |
| 离线转写 | VAD → ASR → 标点恢复，全本地 CPU |
| 说话人分离 | 系统轨做 diarization；麦克风轨恒为本机用户 |
| 说话人命名 | 手动映射 `spk_N` → 真名，回填逐字稿与纪要 |
| Scratchpad 速记 | 会中边听边记，自动保存 |
| 逐字稿校对 | 可编辑，低置信片段高亮 |
| 纪要生成 | 单一内置模板 + OpenAI 兼容端点，长会议走 map-reduce |
| 本地加密存储 | SQLCipher + OS Keychain 托管主密钥 |
| 出网开关 | 「仅本地端点 / 允许任意端点」两档，默认仅本地 |
| 导出 | Markdown |

**MVP 明确不做**（列在这里是为了防止范围蠕变，不是"以后也不做"）

macOS 支持 · 实时字幕 · 跨会议声纹库 · 模板管理 UI（模板改 YAML 文件即可）· 纪要版本树 · DOCX/PDF 导出 · PII 脱敏 · 审计日志导出 · 端点白名单策略 · 热词词典 · 回声消除 · 音频文件加密（只加密数据库）· 多任务并发队列（单任务串行）· 会议平台 SDK 集成 · 服务端/多租户 · 移动端 · 模型训练或微调

### 1.4 MVP 成功标准

四周 dogfood 结束时，同时满足：

1. **能用**：≥ 40 场真实会议全程录制无数据丢失。
2. **够准**：逐字稿人工可读、说话人分离结果经手动命名后可用（量化门槛见 §7）。
3. **有价值**：≥ 60% 的会议，使用者认为生成的纪要「比自己手写省时间」（每场一个二选一打分，不做复杂问卷）。
4. **决策依据充分**：§7 的指标表全部填上实测值，§10.3 的未核实项全部关闭。

达标 → 进入 v1.0 企业化（§9）。不达标 → 用实测数据决定是换模型、换架构，还是停。

---

## 2. 架构

```
┌──────────────────────────────────────────────────────────────────────┐
│  UI 层  Tauri 2 WebView (React + TypeScript)                          │
│   录制控制台 │ Scratchpad 速记 │ 逐字稿校对 + 说话人命名 │ 纪要视图    │
└───────────────────────────────┬──────────────────────────────────────┘
                                │ Tauri IPC (command + event)
┌───────────────────────────────▼──────────────────────────────────────┐
│  Core 层  Rust（与 UI 同进程，无 Python、无 sidecar）                  │
│                                                                       │
│  ┌────────────┐  ┌──────────────┐  ┌───────────┐  ┌───────────────┐  │
│  │ capture/   │  │ jobs/        │  │ asr/      │  │ summarize/    │  │
│  │ WASAPI 双轨│→ │ 单任务串行    │→ │sherpa-onnx│→ │ map-reduce    │  │
│  │ 分片/重采样│  │ 进度事件      │  │ VAD/ASR   │  │ 模板渲染      │  │
│  │ 崩溃恢复    │  │ 断点续跑      │  │ diarize   │  │ 出网开关      │  │
│  └────────────┘  └──────────────┘  │ punc      │  └───────┬───────┘  │
│                                     └───────────┘          │          │
│  ┌──────────────────────────────────────────────────┐      │          │
│  │ store/  SQLCipher（元数据/逐字稿/笔记/纪要）      │      │          │
│  │         + OS Keychain（主密钥 / API Key）         │      │          │
│  └──────────────────────────────────────────────────┘      │          │
└────────────────────────────────────────────────────────────┼──────────┘
                                                             │ 仅文本
                                                  ┌──────────▼──────────┐
                                                  │ OpenAI 兼容端点      │
                                                  │ 默认：本地 Ollama    │
                                                  └─────────────────────┘
```

**两个架构决定及其理由**

**推理跑在 Rust 进程内，不引入 Python。** 曾考虑过 PyTorch + funasr 方案，但那会让安装包从数十 MB 膨胀到 GB 级，还要管理 sidecar 进程的生命周期与崩溃恢复——与选择 Tauri 的理由自相矛盾。已核实 crates.io 上的 `sherpa-onnx` v1.13.6 是官方 crate（owner 即 sherpa-onnx 维护者，Apache-2.0），默认静态链接、首次构建自动拉取平台原生库；官方文档还有专门的 Tauri Desktop Apps 章节。

**ASR 与说话人分离解耦，不追求单管道输出。** sherpa-onnx 的 `OfflineSpeakerDiarization` 本就是独立 API，两条结果按时间轴对齐即可。解耦的额外好处是两侧可以独立替换、独立评测——MVP 阶段这比"一步到位"重要得多。

---

## 3. 技术选型

版本为 2026-08-27 核实时的最新稳定版，W1 结束时落锁到 `docs/dependencies.lock.md`，`Cargo.lock` 与 `pnpm-lock.yaml` 入库。

| 模块 | 选型 | 版本 / 依据 |
| :--- | :--- | :--- |
| 客户端外壳 | Tauri 2（Rust + React 18 + TS） | `tauri` 2.11.5 |
| 推理运行时 | sherpa-onnx Rust crate | `sherpa-onnx` 1.13.6，Apache-2.0，默认 `static` 静态链接 |
| ASR | SenseVoice int8（W1 bake-off 定选） | 模型 `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09`；Rust 示例 `sense_voice.rs` |
| ASR 备选 | Paraformer-zh / FireRedASR-CTC / zipformer-zh-en | 均有现成 Rust 示例；备选存在的原因见 §8 许可证风险 |
| VAD | Silero VAD | Rust 示例 `silero_vad_remove_silence.rs`（另有 TEN VAD、FSMN-VAD 可选） |
| 说话人分离 | `OfflineSpeakerDiarization` = pyannote-segmentation-3.0 + 3D-Speaker embedding + FastClustering | Rust 示例 `offline_speaker_diarization.rs` |
| 标点恢复 | CT-Transformer offline punctuation | Rust 示例 `offline_punctuation.rs` |
| 音频采集 | `wasapi` crate 的 loopback capture | `wasapi` 0.24.0（明确支持 loopback，另附 AEC 示例，MVP 不用） |
| 重采样 | `rubato` | **0.16.x**（5.0 换用 audioadapter 新 API，`SincFixedIn` 已移除，不适用） |
| 数据库 | ~~SQLite + SQLCipher~~ → **SQLite + 字段级 AES-256-GCM** | `rusqlite` 0.40.2 feature `bundled` + `aes-gcm` 0.10。**实现期偏差**：SQLCipher 的 vendored-OpenSSL 需从源码编译 OpenSSL，本机 Windows 工具链缺 Perl 模块无法构建。详见 §14.2 |
| 密钥托管 | OS Keychain | `keyring` 4.1.6（Windows Credential Manager） |
| 模板引擎 | minijinja | 2.24.0（`{{ }}` 定界，不与 Markdown / 提示词里的 `{}` 冲突） |
| token 计数 | tiktoken-rs | 0.12.0 |
| LLM 接入 | 自研 OpenAI-Compatible 客户端（reqwest + SSE） | 覆盖 Ollama / vLLM / DeepSeek / OpenAI 兼容层 / Qwen |

> **不用 cpal 采集的原因**：cpal 的 loopback 支持只覆盖 macOS CoreAudio（且要求 14.6+），其 CHANGELOG 中未见 WASAPI loopback。Windows 侧必须用 `wasapi` crate。这一条在 v1/v2 草案里都写错过，已核实纠正。

---

## 4. 模块设计

### 4.1 音频采集

**双轨策略**：一次会议产出两个 16kHz / 16bit / 单声道 WAV。

- `mic.wav` — 麦克风输入，说话人恒为本机用户。
- `sys.wav` — 系统回环，包含远端全部参会者。

diarization 只在 `sys.wav` 上做，`mic.wav` 直接标注为已知说话人，最后按时间轴归并成单条对话流。这样既提升分离准确率，又完全免去声纹注册环节——**这是本方案里性价比最高的一个设计**，用录制方式的调整换掉了一整个功能模块。

**必须处理的现实问题**（每一条都能在真实会议里把录音毁掉）：

- 采样率不一致：设备原生 44.1k/48k → `rubato` 统一重采样至 16k。
- 会中切换音频设备（插拔耳机、蓝牙断连）：监听设备变更，重开流并在时间轴打断点标记。
- 分片落盘：每 60s 一个分片 + 元数据行，崩溃/断电最多丢 60s，重启后可续录并自动拼接。
- 磁盘水位：低于 2GB 告警并可自动停录；双轨 16kHz 约 220MB/小时。
- 长会议（>4h）全程流式处理，不整文件入内存。

### 4.2 推理管线

```
sys.wav ─┬─→ [VAD] ─→ 语音段 ─→ [ASR] ─→ 段文本 + 词级时间戳
         └─→ [Segmentation + Embedding + 聚类] ─→ (start, end, spk)
                              │
                    时间轴对齐（按重叠时长归属）
                              ▼
mic.wav ─→ [VAD] ─→ [ASR] ─→ 段文本（spk 固定为本机用户）
                              ▼
                  归并排序 → [标点恢复] → [说话人合并]
                              ▼
                      结构化逐字稿（写入 DB）
```

**时间信息**：sherpa-onnx 的 `OfflineRecognizerResult` 含 `text`、`tokens`、`timestamps: Option<Vec<f32>>`、`durations: Option<Vec<f32>>`——词级时间戳可得，说话人切换点可落在词边界而非整段边界。是否返回 `Some` 随模型而定，W1 须对候选模型分别实测；若为 `None` 则退化为段级对齐，设计仍成立。

**对齐规则**：ASR 段与 diarization 段按时间重叠比例归属。重叠 < 50% 或跨越说话人边界的段落标记 `low_confidence`，UI 高亮提示校对。这是真实存在的误差来源，产品上用「可校对」消化，而不是假装没有。

**聚类参数**：`FastClusteringConfig` 只有 `num_clusters: i32` 与 `threshold: f32` 两个字段。已知参会人数时传 `num_clusters`（更准，且会议创建时本就可能填了名单）；未知时传 `threshold` 自动估计——§7 的「说话人数估计准确率」指标就是为标定这个阈值而设。

**说话人合并**：相邻同说话人段落仅在**间隔 < 2s** 时合并；中文拼接不插空格。

**接口契约**：

```rust
pub struct Utterance {
    pub id: i64,
    pub speaker_id: String,           // "user" | "spk_0" | "spk_1" ...
    pub speaker_name: Option<String>, // 手动命名后回填
    pub start_ms: u32,
    pub end_ms: u32,
    pub text: String,
    pub source: Source,               // Mic | System
    pub low_confidence: bool,
}

pub trait TranscriptionEngine: Send + Sync {
    fn transcribe(
        &self,
        job: &AudioJob,
        progress: &dyn Fn(Progress),
        cancel: &CancellationToken,
    ) -> Result<Vec<Utterance>, EngineError>;
}
```

`TranscriptionEngine` 这层抽象是刻意保留的：W1 的多个候选模型实现同一 trait，选型结论变化时上层零改动。这也是 §8 许可证风险的兜底。

### 4.3 说话人命名

1. `mic.wav` 侧自动确定为本机用户。
2. 会议创建时可预填参会者名单（手输或粘贴日历邀请）。
3. 转写完成后，UI 为每个 `spk_N` 展示 3 条代表性发言 + 试听按钮，用户点选映射到名单。
4. 命名结果写入 `speakers` 表并回填全部 utterance 与已生成纪要。

MVP 到此为止。跨会议声纹自动匹配留到 v1.0——它涉及生物特征数据存储，会引入额外合规义务，不适合在验证阶段引入。

### 4.4 纪要编排

**长文本策略**。2 小时会议逐字稿约 60k–90k token，多数本地模型放不下：

```
逐字稿 → 按 token 预算切 chunk（相邻重叠 300 token）
       → [Map]   每 chunk 提取：要点 / 决议候选 / 待办候选 / 争议点，均带 utterance_id
       → [Reduce] 跨 chunk 去重归并，冲突项保留双方并标注「存在分歧」
       → [Compose] 结合 Scratchpad 生成最终纪要
```

- chunk 目标大小 = `min(模型上下文 × 0.5, 8000 token)`；`tiktoken-rs` 估算，非 OpenAI 模型按 1.15 系数留边际。
- 逐字稿 + 笔记合计小于上下文一半时，跳过 map-reduce 直接 Compose。
- **每条决议与待办携带 `utterance_id`，UI 上可点击跳回逐字稿原文。** 这是对抗幻觉最有效的产品手段——比在提示词里写「禁止幻觉」有用得多，也让 §7 的「可溯源比例」变成可量化指标。

**模板**（`templates/meeting_default.yaml`，MVP 只有这一个，改文件即生效，不做管理 UI）：

```yaml
version: "1.0.0"
metadata:
  template_id: general_meeting
  name: 通用会议纪要
  stage: compose            # map | reduce | compose
system_instruction: |
  你是资深企业速记官。结合【参会者速记笔记】与【逐字稿（含说话人标签与 utterance_id）】，
  生成事实准确的会议纪要。

  ### 核心原则
  1. 以逐字稿为事实基准。速记笔记指示用户关注的重点，但具体数据、方案、结论必须与逐字稿交叉核验。
  2. 区分「讨论中的假设」与「最终决议」，不得混淆。
  3. 每条决议与待办后附引用标记 [#utterance_id]，无法定位来源的内容不得写入。
  4. 逐字稿中标记 (?) 的低置信片段，涉及关键数据时须写明「（原文识别存疑，待核对）」。

  ### 输出格式（Markdown）
  # {{ meeting_title }}
  **时间**：{{ meeting_time }} ｜ **参会**：{{ attendees | join('、') }}

  ## 关键决议
  - 结论 [#12]

  ## 核心议题讨论
  ### 议题：xxx
  - 各方观点：……
  - 共识/方案：……

  ## 遗留问题与风险

  ## 待办事项
  - [ ] **@责任人**：任务动作（截止：日期或「待定」）[#87]
user_template: |
  ## 参会者速记笔记
  {{ scratchpad | default("（无）") }}

  ## 逐字稿
  {% for u in utterances %}
  [#{{ u.id }}] {{ u.speaker_name | default(u.speaker_id) }} ({{ u.start_ms | ts }}): {{ u.text }}{% if u.low_confidence %} (?){% endif %}
  {% endfor %}
generation:
  temperature: 0.2
  max_tokens: 4096
```

> 用 minijinja 而非字符串格式化，是因为提示词正文里同时存在「要替换的占位符」和「给模型看的字面量花括号」——用 `str.format` 一类的方案在 `{责任人}` 上必然抛错。模板加载时对必需变量做静态检查，缺失在启动阶段就报错。

### 4.5 LLM 客户端

以下每一条都是 OpenAI 兼容网关上真实会踩的坑，MVP 阶段就要写对，否则 dogfood 期间会被杂音淹没：

- `choices` 可能是空数组（usage-only chunk、首个 role chunk）→ 跳过，不要直接索引 `[0]`。
- SSE 行解析要处理多行 `data:`、`:` 注释行、`\r\n`、跨 chunk 截断的半行。
- JSON 解析失败跳过并计数，连续失败超阈值才中止。
- 错误响应先读出 body 再抛错，日志里带上网关的实际错误信息（只有状态码时排障成本极高）。
- 请求头补 `Accept: text/event-stream`；支持取消令牌（点「停止生成」立即断流）。
- 重试只对 429 / 5xx / 连接错误做指数退避（3 次），4xx 直接失败。
- 首字节超时（30s）与整体超时（默认 300s）分开设置。
- 设置页「测试连接」发一次极短的非流式请求，明确区分鉴权失败、模型名错误、网络不通。

### 4.6 存储

```
meetings(id, title, started_at, ended_at, duration_ms, status, created_at)
audio_chunks(id, meeting_id, source, seq, path, start_ms, duration_ms, sha256)
utterances(id, meeting_id, speaker_id, start_ms, end_ms, text, source, low_confidence)
speakers(id, meeting_id, speaker_key, display_name)
notes(id, meeting_id, content, updated_at)
summaries(id, meeting_id, model, content, token_usage, created_at)
jobs(id, meeting_id, kind, state, progress, error, attempts, created_at, updated_at)
```

- Scratchpad 每 3s 或失焦时落盘——崩溃不丢笔记，这是使用者最不能容忍的丢失。
- MVP 用手写迁移脚本即可，`refinery` 留到表结构稳定后再引入。

---

## 5. 隐私边界

即便是内部 dogfood 版本，这条边界也要从第一天就成立——否则同事不会拿真实会议来试。

| 数据 | 是否出本机 |
| :--- | :--- |
| 原始音频、音频分片 | **永不出本机**，无任何代码路径可上传 |
| 逐字稿全文 | 仅在用户点击「生成纪要」且端点通过开关校验时发送 |
| Scratchpad 笔记 | 同上 |
| 说话人真实姓名 | **替换为 A/B/C 发送**，生成后本地回填真名 |
| 遥测 / 崩溃日志 | 无自动上报 |

**出网开关**：默认 `local_only`（只允许 `127.0.0.1` / `localhost` 端点，其他在 UI 上直接禁用）；可切到 `open`（任意端点）。MVP 不做域名白名单、不做 PII 脱敏正则、不做审计 CSV 导出——但所有对外 HTTP 出口**必须收敛到单一模块**，并把端点、模型、字符数写进本地日志（不记内容）。这层收口现在做是几十行代码，事后补要翻遍整个代码库。

**密钥**：SQLCipher 主密钥首次启动随机生成后存入 Windows Credential Manager，DB 文件本身不含密钥材料；LLM API Key 同样存 Keychain，DB 里只存端点、模型名与引用。MVP 不加密音频文件（只加密数据库），这是明确的取舍，v1.0 补上。

---

## 6. 可靠性

| 场景 | 处理 |
| :--- | :--- |
| 录制中崩溃 / 断电 | 分片落盘 + `jobs` 状态机，重启后提示「恢复上次录制」，最多丢 60s |
| 转写中途失败 | 以「段」为粒度记录进度，重试从最后完成段继续 |
| 模型文件缺失 / 损坏 | 启动时校验 SHA256，失败给出明确修复指引 |
| 磁盘写满 | 阈值告警 → 自动停录并保全已有分片 |
| LLM 端点不可用 | 逐字稿已完整落库，纪要可稍后重试，不影响任何已有数据 |
| 音频设备中途变更 | 自动重开流，时间轴打断点，UI 提示该时刻可能有缺失 |

任务队列单并发串行——多任务争抢 CPU 会让整机卡顿，而 MVP 没有并发的实际需求。进度通过 Tauri event 推到 UI，粒度为「阶段 + 百分比 + 预估剩余」。

---

## 7. 验收指标

**下表全部是目标值，不是实测值。** 未找到可直接引用的官方基准（sherpa-onnx 官方只提供 RK3588 等特定硬件的速度数据，与企业 x86 笔记本不可类比），因此 W1 的第一件事是建测试集，dogfood 结束时用实测值替换整张表。任何一个数字在填入实测值之前，都不得在对外材料中使用。

**测试集**（没有它，后续所有结论都不可信）：

- Set-A：内部真实会议录音 10 场 × 30–60 分钟，覆盖 2–8 人、普通话 + 带口音、中英混说，人工转写并标注说话人边界作为 golden。
- 场景切片：笔记本内置麦（近场）、会议室全向麦（远场）、线上会议（系统回环）。

| 指标 | 目标 | 实测 | 说明 |
| :--- | :--- | :--- | :--- |
| CER（近场） | ≤ 8% | 待填 | 主 KPI |
| CER（远场 / 会议室麦） | ≤ 15% | 待填 | 已知劣化场景，单独统计 |
| DER（`sys.wav`） | ≤ 15% | 待填 | 含 miss / false alarm / confusion |
| 说话人数估计准确率 | ≥ 80%（±0 人） | 待填 | 聚类阈值调参依据 |
| 处理速度 RTF | ≤ 0.15（8 核 x86 CPU） | 待填 | 1 小时音频 ≤ 9 分钟 |
| 峰值内存 | ≤ 2.5GB | 待填 | 全管线常驻 |
| 安装包体积 | ≤ 600MB（含权重） | 待填 | 超出则权重分离下载 |
| 纪要可溯源比例 | ≥ 95% | 待填 | 抽检 20 场，逐条核对 `utterance_id` 引用 |
| 「比手写省时间」比例 | ≥ 60% | 待填 | 每场一次二选一打分 |

---

## 8. 排期与风险

### 8.1 六周排期

| 周 | 目标 | 出口标准 |
| :--- | :--- | :--- |
| **W1** | 选型 bake-off 与基线 | 测试集 + 自动评测脚本（CER/DER/RTF 一键出报告）就绪；候选 ASR 各跑一版并出数据；主选确定；许可证初判完成（§8.2）；**Gemini 3.5 Transcribe 参照测试（§13，需先取得录音者同意）**；依赖落锁 |
| **W2–W3** | Windows 采集 | 双轨录制、16k 重采样、60s 分片、设备热切换、磁盘水位、崩溃恢复。**出口**：3 场 ≥1 小时真实会议零丢帧，两轨时间轴对齐误差 < 200ms，断电测试可恢复 |
| **W4** | 推理管线集成 | diarization 与 ASR 对齐、标点、低置信标记、任务队列与进度事件、分段续跑。**出口**：Set-A 全量跑通，指标不低于 W1 基线（防止工程集成引入劣化） |
| **W5** | 客户端与存储 | Scratchpad、逐字稿校对与说话人命名回填、SQLCipher + Keychain、录制控制台、会议列表、Markdown 导出。**出口**：完整走通「开会 → 记笔记 → 转写 → 命名 → 校对 → 导出」 |
| **W6** | 纪要与打包 | 模板引擎、map-reduce、LLM 客户端（§4.5 全部健壮性要求）、出网开关与单点收口、流式渲染、内部安装包。**出口**：2 小时会议在 8k 上下文模型上成功出纪要；`local_only` 下抓包确认零出网 |
| **W7–W10** | dogfood | 4 周、≥40 场真实会议，填满 §7 指标表，产出 v1.0 决策依据 |

W2–W3 给采集 3 周不是保守——真实会议里能毁掉录音的是设备切换、采样率不匹配、独占模式、断电，而不是把 API 调通。这是全项目最容易低估的一段。

### 8.2 风险登记

| 风险 | 影响 | 缓解 |
| :--- | :--- | :--- |
| **FunASR 模型协议含「仅供参考和学习使用」表述** | 高。SenseVoice / Paraformer / FSMN-VAD / CT-Punc 全套适用阿里 FunASR 模型开源协议 v1.1（非 OSI，HF 标 `license: other`），允许使用/修改/分享、要求署名保留模型名，但有上述表述及单方修订条款 | **MVP 为内部试用，暂不构成阻断**；但商业化前必须取得法务书面结论。W1 因此并行评测许可证清洁的备选链路（k2-fsa 自研 zipformer-zh-en、FireRedASR-CTC、Whisper 系，均有现成 Rust 支持），`TranscriptionEngine` trait 保证切换零成本 |
| 中文会议 CER 达不到 8% | 高，动摇核心主张 | W1 提前验证；备选 Paraformer + 后处理术语纠错；这正是 MVP 要回答的问题，不达标是有效结论而非失败 |
| DER 在 4 人以上、有串台时劣化 | 中 | 双轨录音已消解本机侧；辅以手动命名 + 校对 UI；产品定位是「可校对」而非「全自动」 |
| Windows 采集在真实设备上出问题 | 中 | 3 周专项工期；dogfood 覆盖多种麦克风与会议软件 |
| 安装包体积超预期 | 低 | int8 量化；必要时权重分离首启下载 |

**已排除的风险**：pyannote-segmentation-3.0 的商用许可（核实为 **MIT**，可商用）；sherpa-onnx Rust binding 能力缺口（官方 Rust 示例已覆盖 ASR / VAD / diarization / punctuation / speaker embedding 全部所需能力）。

---

## 9. MVP 之后

dogfood 达标后，v1.0 企业化按此优先级展开，**不在 MVP 阶段提前投入**：

1. macOS 支持（cpal CoreAudio loopback 需 14.6+；13–14.5 走 ScreenCaptureKit，更早版本引导 BlackHole 虚拟声卡）
2. 合规体系：`local_only` / `allowlist` / `open` 三档策略、管理员策略文件下发、PII 脱敏、审计日志导出
3. 音频文件加密、卸载彻底清除
4. 模板体系：模板管理 UI、多模板、纪要版本树、DOCX 导出
5. 跨会议声纹库（需先评估生物特征数据的合规义务）
6. 企业术语热词词典（影响 ASR 选型——热词能力是 Paraformer 的优势项，也是 §13 里云端方案明显领先的一项）
7. 可选云端 ASR 引擎（Gemini 3.5 Transcribe，默认关闭，见 §13）
8. 代码签名与公证、内网权重镜像、部署手册

---

## 10. 事实核实记录（2026-08-27）

本方案的库能力与许可证断言均已联网核实。**§10.3 的未核实项，评审中不得当作已知事实使用。**

### 10.1 已核实为真

| 断言 | 结论 | 依据 |
| :--- | :--- | :--- |
| 存在官方 sherpa-onnx Rust crate | 真。crates.io `sherpa-onnx` v1.13.6，Apache-2.0，owner 为 sherpa-onnx 维护者本人 | crates.io API |
| Rust API 覆盖本方案全部所需能力 | 真。`rust-api-examples/examples/` 下存在 `sense_voice.rs`、`paraformer.rs`、`offline_speaker_diarization.rs`、`offline_punctuation.rs`、`silero_vad_remove_silence.rs`、`ten_vad_remove_silence.rs`、`speaker_embedding_extractor.rs` 等 | GitHub master 仓库树 |
| 静态链接、免手工配置原生库 | 真。默认 `static` feature，首次构建自动下载对应平台原生库 | rust-api-examples/README.md |
| diarization 返回 (start, end, speaker) | 真。`OfflineSpeakerDiarization::process()` → `sort_by_start_time()` → `s.start / s.end / s.speaker`；配置为 pyannote 分割 + 说话人 embedding + `FastClusteringConfig` | `offline_speaker_diarization.rs` |
| ASR 结果含词级时间戳 | 真。`OfflineRecognizerResult { text, tokens, timestamps: Option<Vec<f32>>, durations: Option<Vec<f32>> }` | docs.rs `sherpa_onnx` 1.13.6 |
| `FastClusteringConfig` 字段 | 真。仅 `num_clusters: i32` 与 `threshold: f32` | docs.rs |
| sherpa-onnx 有 Tauri 官方支持 | 真。官方文档设有「Tauri Desktop Apps」章节 | k2-fsa.github.io/sherpa |
| 各 crate 存在且为所列版本 | 真。tauri 2.11.5 / cpal 0.18.2 / wasapi 0.24.0 / minijinja 2.24.0 / tiktoken-rs 0.12.0 / rusqlite 0.40.2 / keyring 4.1.6 / refinery 0.9.2 / rubato 5.0.0 | crates.io API |
| rusqlite 支持 SQLCipher | 真。feature `sqlcipher` / `bundled-sqlcipher` / `bundled-sqlcipher-vendored-openssl` | crates.io rusqlite 0.40.2 |
| Windows loopback 可行 | 真，但**不经 cpal**。`wasapi` crate 明确支持 loopback capture，另附 AEC 示例 | wasapi-rs README |
| macOS loopback（v1.0 相关） | 真，但要求 **14.6+**；cpal CHANGELOG 中 loopback 条目全属 CoreAudio，**未见 WASAPI loopback** | cpal README 最低版本表 + CHANGELOG |

### 10.2 许可证状况

| 组件 | 协议 | 判断 |
| :--- | :--- | :--- |
| sherpa-onnx（代码） | Apache-2.0 | 可用 |
| pyannote/segmentation-3.0 | **MIT**（HF 侧 `gated: auto`；sherpa-onnx 在 Releases 分发已转换的 ONNX） | 可用 |
| 3D-Speaker（embedding 模型来源项目） | Apache-2.0 | 可用 |
| cpal / wasapi / rusqlite 等 crate | Apache-2.0 / MIT 系 | 可用 |
| **SenseVoice、Paraformer、FSMN-VAD、CT-Punc** | **FunASR 模型开源协议 v1.1** | **商业化前需法务结论**，见 §8.2 |

### 10.3 未核实 / 仍属假设

1. **§7 的全部指标数字**——目标值，非实测值，须由 W1 + dogfood 产出真实数据替换。
2. **SenseVoice / Paraformer 的 `timestamps` 是否返回 `Some`**——API 层是 `Option`，行为随模型而定，W1 实测。
3. **macOS 13–14.5 的 ScreenCaptureKit 音频路径与 BlackHole 引导**——v1.0 相关，未核实。
4. **各 ASR 模型在中文会议远场场景的相对表现**——无公开可比基准，只能自测。
5. **打包体积、代码签名与公证流程**——未核实。

### 10.4 复核方法

以上结论均可通过公开端点复核：`crates.io/api/v1/crates/<name>`、`docs.rs/<crate>/latest/`、`api.github.com/repos/k2-fsa/sherpa-onnx`、`raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/rust-api-examples/`、`huggingface.co/api/models/<id>`、`k2-fsa.github.io/sherpa/onnx/`。

---

## 11. 仓库结构

```
vocmeet/
├─ docs/                    本方案、依赖锁、许可证核验
├─ src-tauri/
│  ├─ src/
│  │  ├─ capture/           WASAPI 双轨 / 重采样 / 分片
│  │  ├─ asr/               TranscriptionEngine trait + sherpa-onnx 实现 + 对齐
│  │  ├─ summarize/         模板引擎 / map-reduce / LLM 客户端 / 出网收口
│  │  ├─ store/             SQLCipher / keychain
│  │  └─ jobs/              任务队列与状态机
│  └─ models/               ONNX 权重（打包注入，不入库）
├─ src/                     React UI
├─ templates/               提示词模板 YAML
└─ eval/                    测试集清单、评测脚本、历次报告
```

---

## 12. 待决问题

1. **dogfood 范围**：只在开发团队内，还是扩到相邻业务团队？影响真实会议场景的多样性，也影响 §7 指标的代表性。
2. **默认 LLM 端点**：MVP 默认 `local_only` + 本地 Ollama，需要确认参与 dogfood 的机器能跑得动多大的模型——这直接决定 §4.4 的 chunk 大小与纪要质量上限。
3. **W1 备选 ASR 评测投入**：许可证风险在 MVP 阶段不阻断，是否要在 W1 就花时间评测备选链路？建议评测（成本约 1–2 天，换取商业化决策的自由度），但这是个需要拍板的取舍。

---

## 13. 云端 ASR 评估：Gemini 3.5 Transcribe（2026-08-27 调研）

### 13.1 它是什么（已核实）

Google 于近期发布的**专用语音转文字模型**，非 Gemini 通用多模态模型的一个用法，而是独立端点。已在官方文档核实（文档页 `ai.google.dev/gemini-api/docs/transcribe`，末次更新 2026-08-26）：

| 项 | 内容 |
| :--- | :--- |
| 端点 | `gemini-3.5-transcribe`（批量）、`gemini-3.5-transcribe-live`（WebSocket 实时） |
| 状态 | Gemini Developer API 上为 **Stable**；Vertex AI 上为 **Preview**（`gemini-3.5-transcribe-preview`） |
| 语言 | 85+ locale 自动识别，支持句内/句间 code-switching，含简体中文 |
| 说话人分离 | 支持，**最多 8 人；3 人以上的归属为 Experimental** |
| 时间戳 | 词级起止时间。**官方注明：开启后可能降低整体转写准确率** |
| 自定义词表 | 最多 1000 条术语/专名偏置，官方建议 ≤100 条效果最佳 |
| 智能转写 | verbatim / smart 两种模式；smart 模式去口头禅、解析口误自纠（「周二，不对，周三」→「周三」）、自动分段与 ITN |
| 音频时长上限 | 单次请求 1 小时；**开启 diarization 或词级时间戳后降至 30 分钟**（Vertex AI 上更严，**15 分钟**） |
| 价格 | 批量 ~$0.005/分钟（$0.003 音频输入 + $0.002 文本输出）≈ **$0.30/小时**；Live 版 ~$0.009/分钟 |

### 13.2 能力上，它正好命中我们三个痛点

坦率讲，这个模型的能力清单读起来像是照着本方案 §4.2 的难点列的：

1. **自定义词表** = 热词能力。这是本地栈（SenseVoice 路线）**没有**的，我们在 §1.3 把热词列为 MVP 不做、在 §9 列为 v1.0 待补。
2. **词级时间戳 + diarization 一次给全**，省掉 §4.2 里那套「两条管线按时间轴重叠比例对齐 + 低置信标记」的工程。
3. **code-switching 与 smart 模式**，直接覆盖中英混说与口语清理——后者我们原本指望 LLM 在纪要阶段消化。

价格也不构成障碍：一场 1 小时会议约 $0.30（≈¥2.2）；按一个 20 人团队每月 100 会议小时算，约 **$30/月**。相比之下本地方案是「免费但每小时音频要占 9 分钟 CPU」。

### 13.3 但它与 MVP 的定位有三处硬冲突

**冲突一：隐私主张（决定性）**

§1.1 第一条是「原始音频与逐字稿**永不出本机**」。用这个模型意味着**必须把原始音频上传**（经 Files API），不是上传文本——是音频本身。数据条款核实结果：

- **免费层**：Google 明确「使用你提交的内容来改进和开发 Google 产品及机器学习技术」，且「**人工审阅者可能读取、标注、处理你的 API 输入输出**」，条款原文写着「**不要提交敏感、机密或个人信息**」。企业会议录音属于典型的机密信息 → **完全不可用**。
- **付费层**：定价表明确「Used to improve our products: **No**」，不用于训练。但**滥用监控仍会保留 prompt 与响应 55 天**，被安全过滤器标记的内容可由授权 Google 员工人工查看（仅用于政策执行，不用于训练）。

也就是说，即便走付费层，「音频出本机 + 留存 55 天 + 特定情况下可被人工查看」这三件事都成立。这不是配置能解决的，是产品定位层面的冲突。

**冲突二：时长限制与会议场景不匹配**

开启 diarization 或词级时间戳后，Developer API 限 30 分钟、Vertex AI 限 15 分钟。而 VocMeet 的典型场景是 1–2 小时会议。必须切片，而切片会破坏**跨片说话人一致性**——第 1 片的 `spk_0` 与第 2 片的 `spk_0` 不保证是同一个人，需要自己做跨片声纹对齐。这等于把我们用双轨录音（§4.1）刚消掉的那部分复杂度又请了回来。

**冲突三：说话人分离有明确免责**

「最多 8 人，**3 人以上归属为 Experimental**」——而 VocMeet 的目标场景 2–8 人，正好整个落在这个免责区间内。另有「开启词级时间戳可能降低转写准确率」的权衡说明。所以不能假设云端方案在分离质量上必然优于本地方案，这需要实测。

### 13.4 结论与建议动作

**不作为 MVP 主引擎。** 理由不是能力不够，而是它与 MVP 要验证的命题（「本地离线链路是否够用」）正交——用云端跑通不能回答本地是否可行，反而会掩盖问题。

但有两件事值得做：

**动作一：W1 增加一次参照测试（已加入排期）**

在 Set-A 测试集上用 `gemini-3.5-transcribe` 跑一遍，作为**本地方案准确率的上界参照**。成本极低（10 场 × 45 分钟 ≈ 450 分钟 ≈ **$2.3**）。它的价值在于把 §7 那张目标值表变成有参照系的：

- 若本地 CER 8% 而 Gemini 3%，说明本地方案仍有明显优化空间，值得继续调；
- 若差距只有 1–2 个点，说明本地方案已接近该场景的实际可用水位，可以定型；
- 若本地远差且怎么调都追不上，那就是一个需要向上汇报的产品定位问题，而不是工程问题。

这比原计划中「用 Python funasr 做参照实现」有价值得多——funasr 参照只是同族模型的另一种封装，Gemini 是真正独立的上界。

> **执行前置条件（不可省略）**：这一步本身就是把内部会议音频发给 Google。必须 (a) 事先取得该场会议全体录音者的明确同意，(b) 只用付费层账号，(c) 优先选用测试集中不含敏感内容的场次。不满足则跳过该项，用公开中文会议数据集替代。这是合规动作，不是走流程。

**动作二：v1.0 规划为可选云端引擎（默认关闭）**

`TranscriptionEngine` trait（§4.2）已经在那里了，加一个 `GeminiTranscribeEngine` 实现成本很低。面向那些不介意音频出网的客户——这类客户确实存在，尤其是已深度使用 Google Workspace 的团队。但必须满足：

- 默认关闭，开启时以不可跳过的方式明确告知「**原始音频将上传至 Google**」；
- 仅允许付费层凭据，检测到免费层直接拒绝（免费层条款明文禁止提交机密信息）；
- 纳入 §5 出网开关体系——现有的 `local_only` / `open` 二档需扩展为三档：**本地转写 + 本地 LLM** / **本地转写 + 云端 LLM** / **云端转写 + 云端 LLM**，档位含义对用户直白可见；
- 自动切片（≤30 分钟）+ 跨片说话人对齐，由我们这侧补齐。

**动作三：把热词能力的优先级提上来**

这次调研最有用的副产品：自定义词表是云端方案明显领先、而本地栈缺失的一项。企业会议里人名、产品代号、专业术语的识别错误，对纪要可用性的影响可能比整体 CER 更大。若 W1 的参照测试显示两者差距主要来自专名而非普通词，那么本地侧的 Paraformer contextual/seaco 热词路线就应从 v1.0 提前到 MVP 后立即启动。

### 13.5 实时转写的顺带说明

`gemini-3.5-transcribe-live`（~$0.009/分钟）使实时字幕在技术上变得容易。但 §1.3 已明确实时字幕是本期非目标——理由（与「零资源抢占」的离线批处理定位冲突）在云端方案下不再成立，因为算力不在本机。这一条值得在 v1.0 重新评估，但不是现在：MVP 要先证明纪要本身有价值，实时字幕是锦上添花。

### 13.6 本节核实来源

`ai.google.dev/gemini-api/docs/transcribe`（能力、限制、时长上限）、`ai.google.dev/gemini-api/docs/models`（端点与状态）、`ai.google.dev/gemini-api/docs/pricing`（价格与「是否用于改进产品」）、`ai.google.dev/gemini-api/docs/abuse-monitoring`（55 天留存与人工审阅）、`ai.google.dev/gemini-api/terms`（免费层数据使用条款）、`cloud.google.com/vertex-ai/generative-ai/docs/models/gemini/3-5-transcribe`（Vertex 侧状态与 15 分钟限制）。

**未核实**：该模型在中文会议远场、多人串台场景的实际 CER/DER（无公开基准，只能自测——这正是动作一的目的）；企业版 Vertex AI 是否可申请关闭滥用监控留存（部分 Google Cloud 客户有此选项，未核实是否适用于本模型）。

---

## 14. 实现状态（2026-08-27）

### 14.1 已交付与验证程度

代码在本仓库，约 4700 行 Rust（含测试）。**验证程度分三档，如实标注**：

| 组件 | 状态 | 验证方式 |
| :--- | :--- | :--- |
| `crates/core` 核心库 | 可用 | 69 个单元测试通过 |
| `crates/capture` WASAPI 双轨采集 | 可用 | 4 个单元测试 + 本机实录双轨 8 秒 |
| `crates/cli` 命令行全链路 | 可用 | 手工端到端验证，见 §14.3 |
| `src-tauri` + React 桌面 GUI | 可启动，交互未验证 | 编译通过、进程实启存活、MSI/NSIS 安装包已产出 |

**GUI 的验证边界要说清楚**：编译、启动、前端资源加载、Tauri 命令注册都验证过；
但本机环境无法做人机交互，**「每个按钮点下去行为是否正确」没有验证**。
这是 dogfood 第一天要做的事，不是可以跳过的。

界面五页：录制（含会中速记与转写进度）、逐字稿（说话人命名、低置信高亮、就地校对、导出）、
纪要（流式生成）、设置（出网策略、LLM、聚类阈值）、自检。

CLI 保留——它同时是 §8.1 W1 bake-off 的评测载体（`transcribe --json` 可直接喂评测脚本），
且与 GUI 共用同一个数据库。

### 14.2 与方案的偏差

**SQLCipher → SQLite + 字段级 AEAD（安全性实质降级，需 v1.0 修正）**

`rusqlite` 的 `bundled-sqlcipher-vendored-openssl` 需要从源码编译 OpenSSL，
本机 Windows 工具链因 Perl 模块 `Locale::Maketext::Simple` 缺失而构建失败。
改为对敏感列做 AES-256-GCM 加密，密钥仍在 Windows Credential Manager。

- 仍加密：逐字稿正文、速记笔记、纪要正文
- 不再加密：表结构、时间戳、说话人 key、会议标题

MVP 内部试用可接受，但 v1.0 必须改回整库加密。

### 14.3 实测数据（单机单样本，不能替代 §7 的 Set-A 评测）

sherpa-onnx 官方四人中文测试音频，56.9 秒：

| 项 | 实测 | §7 目标 |
| :--- | :--- | :--- |
| RTF | **0.096** | ≤ 0.15 ✓ |
| 双轨时长对齐误差 | **74ms** | < 200ms ✓ |
| 已知人数时说话人数 | **4（准确）** | — |
| 未知人数时说话人数 | **6（过分裂）** | ≥80% 准确 ✗ |
| 低置信标记比例 | 2/6 条 | — |

两条值得注意的结论：

1. **聚类阈值默认 0.5 会过分裂。** 不传 `--speakers` 时，4 人音频被切成 6 个说话人标签。
   传 `--speakers 4` 则精确得到 4 个。这**实证了 §4.2 的设计判断**（已知人数走 `num_clusters`
   更准），也说明 §7 的「说话人数估计准确率」指标必须在 W1 优先标定——
   否则未填参会人数的场景下分离质量不可用。
2. **RTF 已达标，但这是 57 秒音频的单点数据**，长会议下模型加载摊薄、内存增长的表现未测。

**CER / DER 仍然是空白。** 计算它们需要人工转写并标注说话人边界的 golden 数据，
即 §7 的 Set-A 测试集——这是 W1 的第一件事，尚未开始。

### 14.4 下一步

1. 建 Set-A 测试集与评测脚本，把 §7 指标表的「待填」列填上（W1 首要任务）
2. 标定聚类阈值（§14.3 结论 1）
3. GUI 逐项人工验证（dogfood 第一天）
4. Gemini 3.5 Transcribe 参照测试（§13.4 动作一，需先取得录音者同意）
