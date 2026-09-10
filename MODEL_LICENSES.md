# 模型与第三方依赖许可证说明 (Model & Third-Party Licenses)

VocMeet（声会）的离线语音识别与说话人分离能力基于开源 ONNX 模型生态构建。
软件本身遵循 **Apache-2.0** 许可证开源，运行所需的模型权重不随代码库直接分发，而是在首次使用时按需下载。

各组件与模型的原始版权、开源许可证及商业化使用约束说明如下：

---

## 1. 核心 AI 权重许可证清单

| 模型组件 | 资产名称与来源 | 原始许可证 | 商用约束与合规注意 |
| :--- | :--- | :--- | :--- |
| **语音识别 (ASR)** | SenseVoice Small (阿里开源 / FunASR)<br>`sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8` | **FunASR 模型开源协议 v1.1** | ⚠️ 非标准 OSI 协议。协议含“仅供参考和学习使用”表述，并保留单方修订权。个人学习、学术研究与内部使用无阻碍；**若进行商业化软件分发或 SaaS 服务，须先取得法务合规结论**，或通过 `TranscriptionEngine` trait 切换至清洁许可证模型。 |
| **标点恢复 (Punctuation)** | CT-Transformer (阿里开源 / FunASR)<br>`sherpa-onnx-punct-ct-transformer-zh-en-vocab272727` | **FunASR 模型开源协议 v1.1** | ⚠️ 同上，属阿里 FunASR 模型协议约束。 |
| **静音检测 (VAD)** | Silero VAD<br>`silero_vad.onnx` | **MIT License** | ✅ 宽松商用许可，保留版权声明即可。 |
| **说话人分段 (Segmentation)** | pyannote.audio segmentation 3.0<br>`sherpa-onnx-pyannote-segmentation-3-0` | **MIT License** | ✅ 宽松商用许可，保留版权声明即可。 |
| **声纹特征提取 (Embedding)** | 3D-Speaker eres2net (达摩院开源)<br>`3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k` | **Apache-2.0** | ✅ 宽松商用许可，遵循 Apache-2.0 条款即可。 |

---

## 2. 运行时推理引擎

- **sherpa-onnx** ([github.com/k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx))：
  - 遵循 **Apache-2.0** 许可证。
  - 本项目通过 `crates/core` 的 Rust FFI 静态链接或绑定 sherpa-onnx 运行时。

---

## 3. 音频解码与核心第三方库

- **symphonia** ([github.com/pdeljanov/Symphonia](https://github.com/pdeljanov/Symphonia))：
  - 遵循 **MPL-2.0 (Mozilla Public License 2.0)** 许可证。
  - 说明：MPL-2.0 属于文件级弱著佐权协议。静态链接进 Apache-2.0 应用程序在法律界被广泛认定为兼容；本项目仅直接调用其 API，未对其自身源码进行修改，不构成传染，用户无需开放整个工程源码。
- 其余 Rust / React 核心依赖（`tauri`, `tokio`, `rusqlite`, `aes-gcm`, `react` 等）均遵循 MIT 或 Apache-2.0 许可证。
