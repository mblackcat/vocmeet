# macOS 双轨采集 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `crates/capture` 在 macOS 14.6+ 上具备与 Windows 一致的双轨采集能力（麦克风轨 + 系统输出轨分开落盘），并在系统录音权限缺失导致「静默录成空音频」时主动告警。

**Architecture:** 新增 `macos_impl` 模块，两条轨都经 cpal：麦克风取默认输入设备，系统轨对默认**输出**设备调 `build_input_stream`——cpal 检测到该设备不支持输入后会自动创建 CoreAudio process tap 与聚合设备。采集回调只做一次拷贝并投递到 channel，重采样在主循环批量进行（复用 `vocmeet_core::audio` 既有函数）。

**Tech Stack:** Rust 1.98, cpal 0.18.2, vocmeet-core（downmix / resample_to_target / ChunkWriter）

**设计依据：** `docs/superpowers/specs/2026-09-11-macos-capture-design.md`

**已核实的 cpal 0.18.2 API（写代码时按这个来，勿凭记忆）：**
- `SampleRate = u32`、`ChannelCount = u16`（都是类型别名，**不是 newtype**，无需 `.0`）
- `DeviceTrait::description() -> Result<DeviceDescription, Error>`，再 `.name() -> &str`。**0.18 没有 `Device::name()`**
- `DeviceTrait::build_input_stream<T,D,E>(config: StreamConfig, data_cb: D, err_cb: E, timeout: Option<Duration>)`
- data 回调签名：`FnMut(&[T], &cpal::InputCallbackInfo)`
- `StreamTrait::play()` **必须调用**——`build_*_stream` 返回的流是停止状态
- `SupportedStreamConfig::{config(), channels(), sample_rate()}`
- `HostTrait::{default_input_device, default_output_device, input_devices, output_devices}`

---

### Task 1: 抽出跨平台的采样转换 helper

Windows 的 `decode_and_downsample` 尾段（downmix → resample）macOS 要原样复用。先提成模块级函数，避免两处各写一份。

**Files:**
- Modify: `crates/capture/src/lib.rs`

- [ ] **Step 1: 写失败测试**

加到 `crates/capture/src/lib.rs` 的 `mod tests` 里：

```rust
#[test]
fn to_target_pcm_downmixes_stereo() {
    // 立体声 [左=1.0,右=0.0] 两帧，已是 16k 无需重采样 → 单声道两个 0.5
    let interleaved = [1.0f32, 0.0, 1.0, 0.0];
    let pcm = to_target_pcm(&interleaved, 2, TARGET_SAMPLE_RATE).unwrap();
    assert_eq!(pcm.sample_rate, TARGET_SAMPLE_RATE);
    assert_eq!(pcm.samples, vec![0.5, 0.5]);
}

#[test]
fn to_target_pcm_passes_mono_through() {
    let mono = [0.25f32, -0.25];
    let pcm = to_target_pcm(&mono, 1, TARGET_SAMPLE_RATE).unwrap();
    assert_eq!(pcm.samples, vec![0.25, -0.25]);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p vocmeet-capture to_target_pcm`
Expected: FAIL —— `cannot find function `to_target_pcm` in this scope`

- [ ] **Step 3: 写实现**

加到 `crates/capture/src/lib.rs`，放在 `DeviceInfo` 定义之后、`#[cfg(windows)] mod windows_impl` 之前：

```rust
/// 交错多声道 f32 → 单声道 16k。两个平台采到的原始数据都要过这一步。
///
/// 注意 `resample` 每次调用都会新建一个 `SincFixedIn`（sinc_len 256），
/// 开销不小——调用方必须先攒够一批再调，不要每个回调都来一次。
fn to_target_pcm(interleaved: &[f32], channels: usize, native_rate: u32) -> Result<Pcm> {
    let mono = vocmeet_core::audio::downmix(interleaved, channels);
    let pcm = Pcm {
        sample_rate: native_rate,
        samples: mono,
    };
    Ok(vocmeet_core::audio::resample_to_target(&pcm)?)
}
```

- [ ] **Step 4: 让 Windows 实现改用它**

把 `windows_impl` 里的 `decode_and_downsample` 整个函数体替换为：

```rust
    /// 原始字节 → f32 交错 → 降混单声道 → 重采样到 16k。
    fn decode_and_downsample(bytes: &[u8], native_rate: usize) -> Result<Pcm> {
        let interleaved: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        super::to_target_pcm(&interleaved, CAPTURE_CHANNELS, native_rate as u32)
    }
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p vocmeet-capture`
Expected: PASS，6 个测试（原 4 个 + 新 2 个）

- [ ] **Step 6: 提交**

```bash
git add crates/capture/src/lib.rs
git commit -m "refactor(capture): 抽出跨平台的 to_target_pcm，Windows 改用之"
```

---

### Task 2: `TrackOutcome` 与静音判定

`capture_track` 目前返回 `Vec<ChunkInfo>`，改为带上峰值幅度，好让 `record_dual_track` 判断是否录成了全静音。

`capture_track` 只在 capture crate 内部被调用（已核实：`grep -rn capture_track crates/cli/src crates/core/src src-tauri/src` 无命中），所以改签名不会外溢。

**Files:**
- Modify: `crates/capture/src/lib.rs`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn silent_track_is_flagged_only_when_it_recorded_something() {
    // 录到了分片但峰值为 0 → 判静音
    assert!(track_is_silent(0.0, 3));
    // 有信号 → 不判
    assert!(!track_is_silent(0.5, 3));
    // 压根没录到分片是另一类错误（设备打不开），不该报「静音」
    assert!(!track_is_silent(0.0, 0));
    // 阈值边界：等于阈值不算静音
    assert!(!track_is_silent(SILENCE_PEAK, 1));
}

#[test]
fn silence_warning_points_at_the_actual_fix() {
    let w = silence_warning();
    assert!(w.contains("静音"), "要说清现象");
    if cfg!(target_os = "macos") {
        assert!(w.contains("系统录音"), "macOS 上必须指向权限设置项");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p vocmeet-capture silent`
Expected: FAIL —— `cannot find function `track_is_silent``

- [ ] **Step 3: 写实现**

加到 `crates/capture/src/lib.rs`（紧接 `to_target_pcm` 之后）：

```rust
/// 低于此峰值视为「全静音」。16bit 量化噪声约 3e-5，取一个略高的门限。
const SILENCE_PEAK: f32 = 1e-4;

/// 一条轨的采集结果。
#[derive(Debug, Default)]
pub struct TrackOutcome {
    pub chunks: Vec<ChunkInfo>,
    /// 整条轨的峰值幅度，用于识别「录成功了但全是静音」。
    pub peak: f32,
}

/// 判定一条轨是否「录到了分片但全是静音」。
///
/// 没录到分片不算——那是设备打不开之类的错误，另有报错路径，
/// 在这里也报「静音」只会盖住真正的原因。
fn track_is_silent(peak: f32, chunk_count: usize) -> bool {
    chunk_count > 0 && peak < SILENCE_PEAK
}

/// 系统轨全静音时给用户的提示。两个平台的成因不同，给的指引也不同。
fn silence_warning() -> String {
    let mut m = String::from("系统音频全程静音，转写会得到空结果。");
    if cfg!(target_os = "macos") {
        m.push_str(
            "若当时确实有声音在播放，请检查「系统设置 → 隐私与安全性 → 系统录音」\
             是否已勾选 VocMeet——未授权时 macOS 会静默录成空音频，既不报错也不弹窗。",
        );
    } else {
        m.push_str("常见原因：输出设备被其他程序独占，或录制期间确实没有声音播放。");
    }
    m
}
```

- [ ] **Step 4: 改 `capture_track` 的返回类型（三个平台模块）**

`windows_impl::capture_track` 签名改为 `-> Result<TrackOutcome>`，并在函数内跟踪峰值。
把原来 `writer.push(&pcm.samples)?;` 的两处（批量循环内、尾部落盘处）各自改成：

```rust
                let pcm = decode_and_downsample(&bytes, native_rate)?;
                peak = peak.max(pcm.samples.iter().fold(0.0f32, |m, s| m.max(s.abs())));
                writer.push(&pcm.samples)?;
```

在 `let mut raw: VecDeque<u8> = VecDeque::new();` 旁边加 `let mut peak = 0.0f32;`，
并把结尾的 `Ok(writer.finish()?)` 改成：

```rust
        Ok(TrackOutcome {
            chunks: writer.finish()?,
            peak,
        })
```

`fallback_impl::capture_track` 签名同步改为 `-> Result<TrackOutcome>`（函数体仍是返回 Err，不用动）。

- [ ] **Step 5: 改 `record_dual_track` 消费新类型并产出警告**

把 `record_dual_track` 里两处 join 的 `Ok(Ok(chunks))` 分支改掉：

```rust
    if let Some(h) = mic_handle {
        match h.join() {
            Ok(Ok(t)) => out.mic_chunks = t.chunks,
            Ok(Err(e)) => out.warnings.push(format!("麦克风轨失败：{e}")),
            Err(_) => out.warnings.push("麦克风轨线程 panic".into()),
        }
    }
    if let Some(h) = sys_handle {
        match h.join() {
            Ok(Ok(t)) => {
                if track_is_silent(t.peak, t.chunks.len()) {
                    out.warnings.push(silence_warning());
                }
                out.system_chunks = t.chunks;
            }
            Ok(Err(e)) => out.warnings.push(format!("系统回环轨失败：{e}")),
            Err(_) => out.warnings.push("系统回环轨线程 panic".into()),
        }
    }
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test -p vocmeet-capture && cargo check --workspace --tests`
Expected: PASS，8 个测试；workspace check 无错误

- [ ] **Step 7: 提交**

```bash
git add crates/capture/src/lib.rs
git commit -m "feat(capture): capture_track 带回峰值，系统轨全静音时写入 warning"
```

---

### Task 3: `probe_loopback` 返回可携带说明的 Ok

自检页需要表达第三种状态：「流能开，但探测期间没采到声音，可能是没给权限」。
探测期间恰好没声音是完全正常的，不该把自检行标红，所以用 Ok + 说明文字承载。

**Files:**
- Modify: `crates/capture/src/lib.rs`
- Modify: `crates/cli/src/main.rs:196`
- Modify: `src-tauri/src/main.rs:168`

- [ ] **Step 1: 改签名**

`windows_impl::probe_loopback` 改为 `-> Result<String>`，结尾的 `Ok(())` 改为 `Ok("系统回环可用".to_string())`。
`fallback_impl::probe_loopback` 改为 `-> Result<String>`（函数体不动，仍返回 Err）。

- [ ] **Step 2: 改 CLI 调用点**

`crates/cli/src/main.rs` 第 196 行附近：

```rust
    match vocmeet_capture::probe_loopback() {
        Ok(msg) => println!("{msg}"),
        Err(e) => println!("不可用 —— {e}"),
    }
```

- [ ] **Step 3: 改 Tauri 调用点**

`src-tauri/src/main.rs` 第 168 行附近：

```rust
    let (loopback_ok, loopback_message) = match vocmeet_capture::probe_loopback() {
        Ok(msg) => (true, msg),
        Err(e) => (false, e.to_string()),
    };
```

- [ ] **Step 4: 确认编译通过**

Run: `cargo check --workspace --tests`
Expected: 无错误

- [ ] **Step 5: 提交**

```bash
git add crates/capture/src/lib.rs crates/cli/src/main.rs src-tauri/src/main.rs
git commit -m "refactor(capture): probe_loopback 返回 Result<String> 以承载可用但可疑的状态"
```

---

### Task 4: 引入 cpal 并实现 macOS 设备枚举

**Files:**
- Modify: `Cargo.toml`（workspace 依赖表）
- Modify: `crates/capture/Cargo.toml`
- Modify: `crates/capture/src/lib.rs`

- [ ] **Step 1: 加依赖**

根 `Cargo.toml` 的 `[workspace.dependencies]` 末尾加一行：

```toml
cpal = "0.18"
```

`crates/capture/Cargo.toml` 末尾加：

```toml
[target."cfg(target_os = \"macos\")".dependencies]
cpal.workspace = true
```

- [ ] **Step 2: 收窄 fallback 的 cfg 条件**

`crates/capture/src/lib.rs` 里把

```rust
#[cfg(not(windows))]
mod fallback_impl {
```

改为

```rust
#[cfg(not(any(windows, target_os = "macos")))]
mod fallback_impl {
```

并把文件末尾的两条 re-export 改为三条：

```rust
#[cfg(windows)]
pub use windows_impl::{capture_track, list_devices, probe_loopback};

#[cfg(target_os = "macos")]
pub use macos_impl::{capture_track, list_devices, probe_loopback};

#[cfg(not(any(windows, target_os = "macos")))]
pub use fallback_impl::{capture_track, list_devices, probe_loopback};
```

同时把 `fallback_impl` 里的报错文案从「MVP 仅支持 Windows」改为更准确的说法：

```rust
    pub fn list_devices() -> Result<Vec<DeviceInfo>> {
        Err(CaptureError::Unsupported(
            "当前平台不支持音频采集（已支持 Windows 与 macOS 14.6+）".into(),
        ))
    }
```

`capture_track` 与 `probe_loopback` 的文案同样改为
`"当前平台不支持音频采集（已支持 Windows 与 macOS 14.6+）"`。

- [ ] **Step 3: 写 macOS 模块骨架与设备枚举**

在 `#[cfg(not(any(windows, target_os = "macos")))] mod fallback_impl` 之前插入：

```rust
#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Device, SupportedStreamConfig};

    /// 自检探测时长：够判断有没有信号，又不至于让自检页卡住。
    const PROBE_MS: u64 = 400;

    /// 每攒够这么多毫秒的原始样本才做一次重采样。
    /// 不能每个回调都做——`resample` 每次都会新建 sinc_len=256 的重采样器。
    const BATCH_MS: usize = 250;

    /// 取设备与其默认格式。
    ///
    /// 系统轨故意取**输出**设备：cpal 发现该设备 `!supports_input()` 时，
    /// 会自动建 CoreAudio process tap + 聚合设备来做回环采集。
    /// 这与 Windows 上「打开 Render 设备、以 Capture 方向初始化」是同一个套路。
    fn device_for(source: Source) -> Result<(Device, SupportedStreamConfig)> {
        let host = cpal::default_host();
        match source {
            Source::Mic => {
                let d = host
                    .default_input_device()
                    .ok_or_else(|| CaptureError::Device("没有默认输入设备（麦克风）".into()))?;
                let cfg = d
                    .default_input_config()
                    .map_err(|e| CaptureError::Device(format!("读取麦克风默认格式失败：{e}")))?;
                Ok((d, cfg))
            }
            Source::System => {
                let d = host.default_output_device().ok_or_else(|| {
                    CaptureError::Device("没有默认输出设备，无法做系统回环".into())
                })?;
                let cfg = d.default_output_config().map_err(|e| {
                    CaptureError::Device(format!("读取输出设备默认格式失败：{e}"))
                })?;
                Ok((d, cfg))
            }
        }
    }

    fn describe(d: &Device) -> Option<String> {
        d.description().ok().map(|desc| desc.name().to_string())
    }

    pub fn list_devices() -> Result<Vec<DeviceInfo>> {
        let host = cpal::default_host();
        let default_in = host.default_input_device().as_ref().and_then(describe);
        let default_out = host.default_output_device().as_ref().and_then(describe);

        let mut out = Vec::new();
        for (label, devices, default_name) in [
            ("输入", host.input_devices().ok(), &default_in),
            ("输出", host.output_devices().ok(), &default_out),
        ] {
            match devices {
                Some(iter) => {
                    for d in iter {
                        if let Some(name) = describe(&d) {
                            let is_default = default_name.as_deref() == Some(name.as_str());
                            out.push(DeviceInfo {
                                name,
                                direction: label.into(),
                                is_default,
                            });
                        }
                    }
                }
                None => out.push(DeviceInfo {
                    name: "<枚举失败>".into(),
                    direction: label.into(),
                    is_default: false,
                }),
            }
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: 确认编译**

Run: `cargo check -p vocmeet-capture --tests`
Expected: 报 `capture_track`/`probe_loopback` 未定义（Task 5、6 补），**不应有其它错误**

- [ ] **Step 5: 提交**

```bash
git add Cargo.toml Cargo.lock crates/capture/Cargo.toml crates/capture/src/lib.rs
git commit -m "feat(capture): 引入 cpal，macOS 设备枚举"
```

---

### Task 5: macOS `capture_track`

**Files:**
- Modify: `crates/capture/src/lib.rs`（`macos_impl` 内）

- [ ] **Step 1: 写实现**

在 `macos_impl` 的 `list_devices` 之后加：

```rust
    /// 把 channel 里取到的交错样本攒批、转成 16k 单声道、落盘，返回这批的峰值。
    fn drain_batches(
        pending: &mut Vec<f32>,
        batch_samples: usize,
        channels: usize,
        native_rate: u32,
        writer: &mut ChunkWriter,
    ) -> Result<f32> {
        let mut peak = 0.0f32;
        while pending.len() >= batch_samples {
            let rest = pending.split_off(batch_samples);
            let batch = std::mem::replace(pending, rest);
            let pcm = to_target_pcm(&batch, channels, native_rate)?;
            peak = peak.max(pcm.samples.iter().fold(0.0f32, |m, s| m.max(s.abs())));
            writer.push(&pcm.samples)?;
        }
        Ok(peak)
    }

    pub fn capture_track(
        source: Source,
        config: &CaptureConfig,
        stop: &StopSignal,
    ) -> Result<TrackOutcome> {
        let (device, dev_cfg) = device_for(source)?;
        let channels = dev_cfg.channels() as usize;
        let native_rate = dev_cfg.sample_rate();

        let prefix = match source {
            Source::Mic => "mic",
            Source::System => "sys",
        };
        let mut writer = ChunkWriter::new(
            &config.out_dir,
            prefix,
            TARGET_SAMPLE_RATE,
            config.chunk_seconds,
        )?;

        // 回调线程 → 主线程。回调里只做一次拷贝，绝不做重采样或 IO。
        let (tx, rx): (_, Receiver<Vec<f32>>) = channel();
        let stream = device
            .build_input_stream::<f32, _, _>(
                dev_cfg.config(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // 主线程提前退出时 send 会失败，忽略即可。
                    let _ = tx.send(data.to_vec());
                },
                |e| tracing::warn!("cpal 采集流错误：{e}"),
                None,
            )
            .map_err(|e| {
                CaptureError::Stream(format!(
                    "打开{}失败：{e}{}",
                    if source == Source::System { "系统回环" } else { "麦克风" },
                    if source == Source::System {
                        "。系统回环需要 macOS 14.6 或更高版本"
                    } else {
                        ""
                    }
                ))
            })?;

        // build_*_stream 返回的流是停止状态，必须 play 才会触发回调。
        stream
            .play()
            .map_err(|e| CaptureError::Stream(format!("启动采集流失败：{e}")))?;

        let batch_samples = (native_rate as usize) * BATCH_MS / 1000 * channels;
        let mut pending: Vec<f32> = Vec::with_capacity(batch_samples * 2);
        let mut peak = 0.0f32;

        while !stop.is_stopped() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(buf) => {
                    pending.extend_from_slice(&buf);
                    peak = peak.max(drain_batches(
                        &mut pending,
                        batch_samples,
                        channels,
                        native_rate,
                        &mut writer,
                    )?);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // 丢掉流 → 回调不再触发 → tx 被释放 → channel 断开。
        drop(stream);
        while let Ok(buf) = rx.try_recv() {
            pending.extend_from_slice(&buf);
        }
        peak = peak.max(drain_batches(
            &mut pending,
            batch_samples,
            channels,
            native_rate,
            &mut writer,
        )?);

        // 落盘尾部不足一批的数据，长度要对齐到声道数。
        let usable = pending.len() - pending.len() % channels.max(1);
        if usable > 0 {
            let pcm = to_target_pcm(&pending[..usable], channels, native_rate)?;
            peak = peak.max(pcm.samples.iter().fold(0.0f32, |m, s| m.max(s.abs())));
            writer.push(&pcm.samples)?;
        }

        Ok(TrackOutcome {
            chunks: writer.finish()?,
            peak,
        })
    }
```

- [ ] **Step 2: 确认编译**

Run: `cargo check -p vocmeet-capture --tests`
Expected: 只剩 `probe_loopback` 未定义

- [ ] **Step 3: 提交**

```bash
git add crates/capture/src/lib.rs
git commit -m "feat(capture): macOS capture_track（cpal 回调 → 批量重采样 → 分片落盘）"
```

---

### Task 6: macOS `probe_loopback`

**Files:**
- Modify: `crates/capture/src/lib.rs`（`macos_impl` 内）

- [ ] **Step 1: 写实现**

```rust
    /// 兼容性检查：能否打开系统回环，以及是否真的采到了声音。
    ///
    /// 探测期间恰好没有声音在播放是正常的，所以「没采到」只返回 Ok + 提示，
    /// 不返回 Err——否则自检页会因为用户当时没放音乐而误报红。
    pub fn probe_loopback() -> Result<String> {
        let (device, dev_cfg) = device_for(Source::System)?;
        let (tx, rx): (_, Receiver<Vec<f32>>) = channel();

        let stream = device
            .build_input_stream::<f32, _, _>(
                dev_cfg.config(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = tx.send(data.to_vec());
                },
                |_| {},
                None,
            )
            .map_err(|e| {
                CaptureError::Stream(format!(
                    "系统回环不可用：{e}。需要 macOS 14.6 或更高版本，\
                     且需在「系统设置 → 隐私与安全性 → 系统录音」中允许 VocMeet。"
                ))
            })?;
        stream
            .play()
            .map_err(|e| CaptureError::Stream(format!("启动系统回环失败：{e}")))?;

        let deadline = Instant::now() + Duration::from_millis(PROBE_MS);
        let mut peak = 0.0f32;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(buf) => peak = peak.max(buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(stream);

        if peak < SILENCE_PEAK {
            Ok(format!(
                "系统回环能打开，但探测 {PROBE_MS}ms 内没采到任何声音。\
                 如果当时确实有音频在播放，请检查「系统设置 → 隐私与安全性 → 系统录音」\
                 是否已勾选 VocMeet——未授权时 macOS 会静默录成空音频。"
            ))
        } else {
            Ok("系统回环可用".to_string())
        }
    }
```

- [ ] **Step 2: 全量检查与测试**

Run: `cargo test -p vocmeet-capture && cargo check --workspace --tests`
Expected: 8 个测试通过，workspace 无错误

- [ ] **Step 3: 提交**

```bash
git add crates/capture/src/lib.rs
git commit -m "feat(capture): macOS probe_loopback，区分打不开与采不到声音"
```

---

### Task 7: 麦克风权限声明与打包配置

没有 `NSMicrophoneUsageDescription` 时 macOS 会直接拒绝麦克风访问。
`bundle.macOS.infoPlist` 已核实是**文件路径**（schema 类型 `['string','null']`，
描述 "Path to a Info.plist file to merge with the default Info.plist"），不是内联对象。

**Files:**
- Create: `src-tauri/Info.plist`
- Modify: `src-tauri/tauri.conf.json`

- [ ] **Step 1: 建 Info.plist**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSMicrophoneUsageDescription</key>
	<string>VocMeet 需要访问麦克风，以录制您本人在会议中的发言。音频仅保存在本机，不会上传。</string>
</dict>
</plist>
```

- [ ] **Step 2: 挂到 tauri.conf.json**

在 `bundle` 对象里、`longDescription` 之后加（注意前一行要补逗号）：

```json
    "macOS": {
      "infoPlist": "Info.plist"
    }
```

**不要设 `minimumSystemVersion`**：录制需要 14.6+，但导入音频、转写、纪要在更老的
macOS 上可用，把整个应用锁死是净损失。

- [ ] **Step 3: 验证配置合法**

Run: `python3 -c "import json; json.load(open('src-tauri/tauri.conf.json')); print('ok')"`
Expected: `ok`

Run: `plutil -lint src-tauri/Info.plist`
Expected: `src-tauri/Info.plist: OK`

- [ ] **Step 4: 提交**

```bash
git add src-tauri/Info.plist src-tauri/tauri.conf.json
git commit -m "feat(app): 声明 macOS 麦克风用途，打包合并 Info.plist"
```

---

### Task 8: 真机验证（只能在 macOS 开发机上做）

CI 的 runner 没有音频设备、也没有 TCC 授权，这一步**无法自动化**。
未做这一步之前，不得在任何文档里把 macOS 采集写成「可用」。

**Files:**
- Modify: `README.md`（据实记录验证结果）

- [ ] **Step 1: 构建 CLI**

Run: `cargo build --release -p vocmeet-cli`
Expected: 编译通过

- [ ] **Step 2: 枚举设备与探测回环**

Run: `./target/release/vocmeet-cli devices`
Expected: 列出输入/输出设备；末行给出系统回环探测结论。
若提示未采到声音，**先放一段音乐再跑一次**，确认能变成「系统回环可用」。
若仍为静音 → 去「系统设置 → 隐私与安全性 → 系统录音」勾选终端/VocMeet，再试。

- [ ] **Step 3: 实录 15 秒双轨**

录制期间**同时**播放音频并对着麦克风说话：

Run: `./target/release/vocmeet-cli record --seconds 15 --out /tmp/vocmeet-mactest`
Expected: 两条轨各 1 个分片，且**没有**静音警告

- [ ] **Step 4: 确认两条轨都有真实信号**

```bash
python3 - <<'PY'
import wave, audioop, glob
for f in sorted(glob.glob('/tmp/vocmeet-mactest/*.wav')):
    w = wave.open(f); n = w.getnframes()
    pcm = w.readframes(n)
    print(f, f"{n/w.getframerate():.2f}s", "peak=", audioop.max(pcm, w.getsampwidth()))
PY
```

Expected: `mic_00000.wav` 与 `sys_00000.wav` 的 peak 都**显著大于 0**（几千以上），
时长都接近 15s，两者相差 <200ms（§8.1 出口标准）。

- [ ] **Step 5: 据实更新 README**

把「已知限制（macOS）」一段改写为实际状态：采集已支持 14.6+、需要系统录音权限、
并写明验证到了什么程度（实录 15 秒双轨通过 / 长会议未测）。
**不得**把「编译通过」写成「可用」——这是仓库既有的文档纪律。

- [ ] **Step 6: 提交**

```bash
git add README.md
git commit -m "docs: 据实记录 macOS 采集的验证状态"
```

---

## 完成标准

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo check --workspace --tests` 无错误
- [ ] CI 三平台（windows / ubuntu / macos）均通过
- [ ] macOS 真机实录双轨，两条轨都有信号，无静音警告
- [ ] README 如实反映验证边界
