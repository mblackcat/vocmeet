//! 音频采集。对应 §4.1。
//!
//! 双轨策略：麦克风轨（本机用户）与系统回环轨（远端参会者）分开录制，
//! 不在采集端混音。这样 diarization 只需处理系统轨，本机用户身份天然确定。
//!
//! **验证状态**：Windows 实现基于 `wasapi` 0.24 编写，编译通过；
//! 但环境所限**未在有真实音频输入/输出的机器上运行验证**。
//! 首次在目标机器上使用请先跑 `vocmeet-cli devices` 与 `vocmeet-cli record --seconds 10`。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use vocmeet_core::audio::{ChunkInfo, ChunkWriter, Pcm};
use vocmeet_core::{Source, TARGET_SAMPLE_RATE};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("平台不支持：{0}")]
    Unsupported(String),
    #[error("音频设备错误：{0}")]
    Device(String),
    #[error("音频流错误：{0}")]
    Stream(String),
    #[error(transparent)]
    Core(#[from] vocmeet_core::Error),
}

pub type Result<T> = std::result::Result<T, CaptureError>;

/// 录制配置。
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// 分片输出目录。
    pub out_dir: PathBuf,
    /// 分片时长（秒）。§4.1：崩溃最多损失一个分片。
    pub chunk_seconds: u32,
    /// 磁盘剩余低于此值即停止录制。
    pub min_free_bytes: u64,
    pub record_mic: bool,
    pub record_system: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("recordings"),
            chunk_seconds: 60,
            min_free_bytes: 2 * 1024 * 1024 * 1024,
            record_mic: true,
            record_system: true,
        }
    }
}

/// 录制停止信号。
#[derive(Clone, Default)]
pub struct StopSignal(Arc<AtomicBool>);

impl StopSignal {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// 一次录制的产出。
#[derive(Debug, Default)]
pub struct Recording {
    pub mic_chunks: Vec<ChunkInfo>,
    pub system_chunks: Vec<ChunkInfo>,
    /// 采集过程中发生的非致命异常（设备切换、丢帧等），供 UI 在时间轴上打标记。
    pub warnings: Vec<String>,
}

/// 音频设备信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub direction: String,
    pub is_default: bool,
}

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

/// 低于此峰值视为「全静音」。16bit 量化噪声约 3e-5，取一个略高的门限。
///
/// 注意这个门限是照着**重采样后**的 16k 单声道信号调的（见 `drain_batches`）。
/// 实时电平表取的是重采样前的交织样本，两者在门限附近并不等价，不要混用。
pub const SILENCE_PEAK: f32 = 1e-4;

/// 一次实时电平采样。`rms` 驱动波形高度，`peak` 留给削波提示。
///
/// 取自**重采样前**的设备原始样本——重采样要攒满 250ms 一批（`BATCH_MS`）才做，
/// 对电平表来说既太粗也太突兀。代价是与 `TrackOutcome::peak` 的口径不同，见 `SILENCE_PEAK`。
#[derive(Debug, Clone, Copy)]
pub struct LevelSample {
    pub source: Source,
    pub rms: f32,
    pub peak: f32,
}

/// 实时电平回调。在采集线程内同步调用，必须廉价且绝不阻塞。
pub type LevelSink = Arc<dyn Fn(LevelSample) + Send + Sync>;

/// 实时电平的推送间隔。20 Hz——够顺滑，又不会打爆 Tauri 的 IPC 桥。
const LEVEL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// 算一批交织样本的 rms 与峰值。空输入返回全零而不是 NaN。
fn level_of(buf: &[f32]) -> (f32, f32) {
    if buf.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f32;
    for &s in buf {
        sum_sq += (s as f64) * (s as f64);
        peak = peak.max(s.abs());
    }
    ((sum_sq / buf.len() as f64).sqrt() as f32, peak)
}

/// 攒够 `LEVEL_INTERVAL` 再推一次电平，避免每个设备回调都发一条事件。
struct LevelMeter<'a> {
    sink: Option<&'a LevelSink>,
    source: Source,
    last: std::time::Instant,
    sum_sq: f64,
    count: usize,
    peak: f32,
}

impl<'a> LevelMeter<'a> {
    fn new(sink: Option<&'a LevelSink>, source: Source) -> Self {
        Self {
            sink,
            source,
            last: std::time::Instant::now(),
            sum_sq: 0.0,
            count: 0,
            peak: 0.0,
        }
    }

    /// 吃进一批原始样本。到点了就推一次并清空累积。
    fn push(&mut self, buf: &[f32]) {
        let Some(sink) = self.sink else { return };
        let (rms, peak) = level_of(buf);
        self.sum_sq += (rms as f64) * (rms as f64) * buf.len() as f64;
        self.count += buf.len();
        self.peak = self.peak.max(peak);

        if self.last.elapsed() < LEVEL_INTERVAL {
            return;
        }
        let rms = if self.count == 0 {
            0.0
        } else {
            (self.sum_sq / self.count as f64).sqrt() as f32
        };
        sink(LevelSample {
            source: self.source,
            rms,
            peak: self.peak,
        });
        self.last = std::time::Instant::now();
        self.sum_sq = 0.0;
        self.count = 0;
        self.peak = 0.0;
    }

    /// 没有数据流过时也要按节奏推零，否则 UI 分不清「静音」和「卡住了」。
    fn tick_idle(&mut self) {
        let Some(sink) = self.sink else { return };
        if self.last.elapsed() < LEVEL_INTERVAL {
            return;
        }
        sink(LevelSample {
            source: self.source,
            rms: 0.0,
            peak: 0.0,
        });
        self.last = std::time::Instant::now();
        self.sum_sq = 0.0;
        self.count = 0;
        self.peak = 0.0;
    }
}

/// 一条轨的采集结果。
#[derive(Debug, Default)]
pub struct TrackOutcome {
    pub chunks: Vec<ChunkInfo>,
    /// 整条轨的峰值幅度，用于识别「录成功了但全是静音」。
    pub peak: f32,
}

/// 判定系统轨是否「没录到有效声音」。两种形态都要告警：
///
/// - **有分片但全是零**：权限缺失时的典型表现，流开得好好的，内容全是静音。
/// - **一个分片都没有**：macOS 实测——CoreAudio 的 process tap 在完全没有音频
///   流过时根本不触发回调，连文件都不会生成。这条路径不抛任何 Err，
///   不在这里兜住的话，用户只会拿到一场空录音而得不到任何提示。
fn system_track_has_no_audio(peak: f32, chunk_count: usize) -> bool {
    chunk_count == 0 || peak < SILENCE_PEAK
}

/// 系统轨没录到声音时给用户的提示。两个平台的成因不同，给的指引也不同。
///
/// 录制结束后的告警与录制中的实时告警共用这一份文案，避免两处说法不一致。
pub fn silence_warning() -> String {
    let mut m = String::from("系统音频没有录到任何声音，转写会得到空结果。");
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

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::collections::VecDeque;
    use wasapi::{
        initialize_mta, DeviceEnumerator, Direction, SampleType, ShareMode, StreamMode, WaveFormat,
    };

    /// 采集用的中间格式：32bit float，设备原生采样率，立体声。
    /// 之后统一降混 + 重采样到 16k 单声道。
    const CAPTURE_BITS: usize = 32;
    const CAPTURE_CHANNELS: usize = 2;

    pub fn list_devices() -> Result<Vec<DeviceInfo>> {
        initialize_mta()
            .ok()
            .map_err(|e| CaptureError::Device(format!("COM 初始化失败: {e}")))?;
        let enumerator =
            DeviceEnumerator::new().map_err(|e| CaptureError::Device(format!("{e}")))?;

        let mut out = Vec::new();
        for (dir, label) in [(Direction::Capture, "输入"), (Direction::Render, "输出")] {
            let default_name = enumerator
                .get_default_device(&dir)
                .ok()
                .and_then(|d| d.get_friendlyname().ok());

            let collection = match enumerator.get_device_collection(&dir) {
                Ok(c) => c,
                Err(e) => {
                    out.push(DeviceInfo {
                        name: format!("<枚举失败: {e}>"),
                        direction: label.into(),
                        is_default: false,
                    });
                    continue;
                }
            };
            let count = collection.get_nbr_devices().unwrap_or(0);
            for i in 0..count {
                if let Ok(dev) = collection.get_device_at_index(i) {
                    if let Ok(name) = dev.get_friendlyname() {
                        let is_default = default_name.as_deref() == Some(name.as_str());
                        out.push(DeviceInfo {
                            name,
                            direction: label.into(),
                            is_default,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    /// 采集一条轨到分片文件。
    ///
    /// `loopback = true` 时打开默认**输出**设备并以 Capture 方向初始化——
    /// 这就是 WASAPI 的系统回环录制（录到的是扬声器正在播放的内容）。
    pub fn capture_track(
        source: Source,
        config: &CaptureConfig,
        stop: &StopSignal,
        level: Option<&LevelSink>,
    ) -> Result<TrackOutcome> {
        initialize_mta()
            .ok()
            .map_err(|e| CaptureError::Device(format!("COM 初始化失败: {e}")))?;

        let loopback = source == Source::System;
        let enumerator =
            DeviceEnumerator::new().map_err(|e| CaptureError::Device(format!("{e}")))?;
        let device_dir = if loopback {
            Direction::Render
        } else {
            Direction::Capture
        };
        let device = enumerator
            .get_default_device(&device_dir)
            .map_err(|e| CaptureError::Device(format!("获取默认设备失败: {e}")))?;

        let mut client = device
            .get_iaudioclient()
            .map_err(|e| CaptureError::Device(format!("获取 IAudioClient 失败: {e}")))?;

        // 用设备混音格式的采样率，交给 WASAPI 做格式转换（autoconvert）。
        let native_rate = client
            .get_mixformat()
            .ok()
            .map(|f| f.get_samplespersec() as usize)
            .unwrap_or(48_000);

        let format = WaveFormat::new(
            CAPTURE_BITS,
            CAPTURE_BITS,
            &SampleType::Float,
            native_rate,
            CAPTURE_CHANNELS,
            None,
        );
        let blockalign = format.get_blockalign() as usize;

        let (_def_period, min_period) = client.get_device_period().unwrap_or((0, 0));
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: min_period,
        };
        client
            .initialize_client(&format, &Direction::Capture, &mode)
            .map_err(|e| {
                CaptureError::Stream(format!(
                    "初始化{}失败: {e}",
                    if loopback { "系统回环" } else { "麦克风" }
                ))
            })?;

        let event = client
            .set_get_eventhandle()
            .map_err(|e| CaptureError::Stream(format!("事件句柄: {e}")))?;
        let capture_client = client
            .get_audiocaptureclient()
            .map_err(|e| CaptureError::Stream(format!("捕获客户端: {e}")))?;

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

        let mut raw: VecDeque<u8> = VecDeque::new();
        let mut peak = 0.0f32;
        let mut meter = LevelMeter::new(level, source);
        client
            .start_stream()
            .map_err(|e| CaptureError::Stream(format!("启动流: {e}")))?;

        while !stop.is_stopped() {
            // 事件超时不致命：静音时设备可能长时间不产生数据。
            if event.wait_for_event(1_000).is_err() {
                meter.tick_idle();
                continue;
            }
            let frames = capture_client.get_next_packet_size().unwrap_or(None).unwrap_or(0);
            if frames == 0 {
                meter.tick_idle();
                continue;
            }
            let before = raw.len();
            if capture_client.read_from_device_to_deque(&mut raw).is_err() {
                continue;
            }

            // 电平取本次新读到的原始样本，不等 250ms 那一批——否则表针只有 4Hz。
            // 多一次拷贝，但 packet 只有 10ms 级别，代价可忽略。
            if level.is_some() && raw.len() > before {
                let fresh: Vec<f32> = raw
                    .iter()
                    .skip(before)
                    .copied()
                    .collect::<Vec<u8>>()
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                meter.push(&fresh);
            }

            // 攒够一批再转换，避免每个 packet 都过一次重采样器。
            let batch_bytes = blockalign * native_rate / 4; // ≈250ms
            while raw.len() >= batch_bytes {
                let bytes: Vec<u8> = raw.drain(..batch_bytes).collect();
                let pcm = decode_and_downsample(&bytes, native_rate)?;
                peak = peak.max(pcm.samples.iter().fold(0.0f32, |m, s| m.max(s.abs())));
                writer.push(&pcm.samples)?;
            }
        }

        let _ = client.stop_stream();

        // 落盘尾部不足一片的数据。
        if !raw.is_empty() {
            let bytes: Vec<u8> = raw.drain(..).collect();
            let usable = bytes.len() - bytes.len() % blockalign;
            if usable > 0 {
                let pcm = decode_and_downsample(&bytes[..usable], native_rate)?;
                peak = peak.max(pcm.samples.iter().fold(0.0f32, |m, s| m.max(s.abs())));
                writer.push(&pcm.samples)?;
            }
        }

        Ok(TrackOutcome {
            chunks: writer.finish()?,
            peak,
        })
    }

    /// 原始字节 → f32 交错 → 降混单声道 → 重采样到 16k。
    fn decode_and_downsample(bytes: &[u8], native_rate: usize) -> Result<Pcm> {
        let interleaved: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        super::to_target_pcm(&interleaved, CAPTURE_CHANNELS, native_rate as u32)
    }

    /// 兼容性检查：能否打开系统回环。UI 首启时调用，失败要给出可操作提示。
    pub fn probe_loopback() -> Result<String> {
        initialize_mta()
            .ok()
            .map_err(|e| CaptureError::Device(format!("COM 初始化失败: {e}")))?;
        let enumerator =
            DeviceEnumerator::new().map_err(|e| CaptureError::Device(format!("{e}")))?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| CaptureError::Device(format!("无默认输出设备: {e}")))?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|e| CaptureError::Device(format!("{e}")))?;
        let rate = client
            .get_mixformat()
            .ok()
            .map(|f| f.get_samplespersec() as usize)
            .unwrap_or(48_000);
        let format = WaveFormat::new(
            CAPTURE_BITS,
            CAPTURE_BITS,
            &SampleType::Float,
            rate,
            CAPTURE_CHANNELS,
            None,
        );
        client
            .initialize_client(
                &format,
                &Direction::Capture,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 0,
                },
            )
            .map_err(|e| {
                CaptureError::Stream(format!(
                    "系统回环不可用：{e}。\
                     常见原因：输出设备被其他程序独占，或当前无可用输出设备。"
                ))
            })?;
        let _ = ShareMode::Shared; // 明确我们只用共享模式
        Ok("系统回环可用".to_string())
    }
}

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

    /// 把攒够的交错样本转成 16k 单声道并落盘，返回这批的峰值。
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
        level: Option<&LevelSink>,
    ) -> Result<TrackOutcome> {
        let (device, dev_cfg) = device_for(source)?;
        let channels = (dev_cfg.channels() as usize).max(1);
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
                let what = if source == Source::System {
                    "系统回环"
                } else {
                    "麦克风"
                };
                let hint = if source == Source::System {
                    "。系统回环需要 macOS 14.6 或更高版本"
                } else {
                    ""
                };
                CaptureError::Stream(format!("打开{what}失败：{e}{hint}"))
            })?;

        // build_*_stream 返回的流是停止状态，必须 play 才会触发回调。
        stream
            .play()
            .map_err(|e| CaptureError::Stream(format!("启动采集流失败：{e}")))?;

        let batch_samples = (native_rate as usize) * BATCH_MS / 1000 * channels;
        let mut pending: Vec<f32> = Vec::with_capacity(batch_samples * 2);
        let mut peak = 0.0f32;
        let mut meter = LevelMeter::new(level, source);

        while !stop.is_stopped() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(buf) => {
                    // 电平走重采样前的原始样本，与下面的 peak 折叠是两套口径，别合并。
                    meter.push(&buf);
                    pending.extend_from_slice(&buf);
                    peak = peak.max(drain_batches(
                        &mut pending,
                        batch_samples,
                        channels,
                        native_rate,
                        &mut writer,
                    )?);
                }
                // 超时说明这段时间一个回调都没来。macOS 上完全没有音频流过时
                // process tap 根本不触发回调，这里要照常推零，UI 才能画出那条直线。
                Err(RecvTimeoutError::Timeout) => meter.tick_idle(),
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
        let usable = pending.len() - pending.len() % channels;
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
}

#[cfg(not(any(windows, target_os = "macos")))]
mod fallback_impl {
    use super::*;

    pub fn list_devices() -> Result<Vec<DeviceInfo>> {
        Err(CaptureError::Unsupported(
            "当前平台不支持音频采集（已支持 Windows 与 macOS 14.6+）".into(),
        ))
    }

    pub fn capture_track(
        _source: Source,
        _config: &CaptureConfig,
        _stop: &StopSignal,
        _level: Option<&LevelSink>,
    ) -> Result<TrackOutcome> {
        Err(CaptureError::Unsupported(
            "当前平台不支持音频采集（已支持 Windows 与 macOS 14.6+）".into(),
        ))
    }

    pub fn probe_loopback() -> Result<String> {
        Err(CaptureError::Unsupported(
            "当前平台不支持音频采集（已支持 Windows 与 macOS 14.6+）".into(),
        ))
    }
}

#[cfg(windows)]
pub use windows_impl::{capture_track, list_devices, probe_loopback};

#[cfg(target_os = "macos")]
pub use macos_impl::{capture_track, list_devices, probe_loopback};

#[cfg(not(any(windows, target_os = "macos")))]
pub use fallback_impl::{capture_track, list_devices, probe_loopback};

/// 同时录制双轨，直到 `stop` 被置位。
///
/// 两条轨各起一个线程——它们的设备时钟不同步，必须独立驱动。
/// 时间轴对齐依赖两者近似同时 start，误差目标 <200ms（§8.1 W2–W3 出口标准）。
pub fn record_dual_track(config: &CaptureConfig, stop: &StopSignal) -> Result<Recording> {
    record_dual_track_with_levels(config, stop, None)
}

/// 与 `record_dual_track` 相同，但额外把实时电平推给 `level`。
///
/// GUI 用它画录制中的双轨波形；CLI 不需要，走上面那个薄包装即可。
pub fn record_dual_track_with_levels(
    config: &CaptureConfig,
    stop: &StopSignal,
    level: Option<LevelSink>,
) -> Result<Recording> {
    let mut out = Recording::default();

    let mic_handle = if config.record_mic {
        let cfg = config.clone();
        let s = stop.clone();
        let lv = level.clone();
        Some(std::thread::spawn(move || {
            capture_track(Source::Mic, &cfg, &s, lv.as_ref())
        }))
    } else {
        None
    };

    let sys_handle = if config.record_system {
        let cfg = config.clone();
        let s = stop.clone();
        let lv = level.clone();
        Some(std::thread::spawn(move || {
            capture_track(Source::System, &cfg, &s, lv.as_ref())
        }))
    } else {
        None
    };

    // 任一轨失败不应让整场录制丢失——另一轨的数据仍然有价值。
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
                if system_track_has_no_audio(t.peak, t.chunks.len()) {
                    out.warnings.push(silence_warning());
                }
                out.system_chunks = t.chunks;
            }
            Ok(Err(e)) => out.warnings.push(format!("系统回环轨失败：{e}")),
            Err(_) => out.warnings.push("系统回环轨线程 panic".into()),
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_track_without_audio_is_flagged() {
        // 有分片但峰值为 0：权限缺失时的典型表现
        assert!(system_track_has_no_audio(0.0, 3));
        // 一个分片都没有：macOS 实测——CoreAudio 的 tap 在完全没有音频流过时
        // 根本不触发回调，于是连文件都不会生成。这条路径没有任何 Err 会被抛出，
        // 不在这里告警的话用户就只能得到一场空录音而毫无提示。
        assert!(system_track_has_no_audio(0.0, 0));
        // 有信号 → 不判
        assert!(!system_track_has_no_audio(0.5, 3));
        // 阈值边界：等于阈值不算静音
        assert!(!system_track_has_no_audio(SILENCE_PEAK, 1));
    }

    #[test]
    fn silence_warning_points_at_the_actual_fix() {
        let w = silence_warning();
        assert!(w.contains("没有录到"), "要说清现象");
        assert!(w.contains("空结果"), "要说清后果");
        if cfg!(target_os = "macos") {
            assert!(w.contains("系统录音"), "macOS 上必须指向权限设置项");
        }
    }

    #[test]
    fn to_target_pcm_downmixes_stereo() {
        // 立体声 [左=1.0,右=0.0] 两帧，已是 16k 无需重采样 → 单声道两个 0.5
        let interleaved = [1.0f32, 0.0, 1.0, 0.0];
        let pcm = to_target_pcm(&interleaved, 2, TARGET_SAMPLE_RATE).unwrap();
        assert_eq!(pcm.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(pcm.samples, vec![0.5, 0.5]);
    }

    #[test]
    fn level_of_computes_rms_and_peak() {
        // 满幅方波：每个样本绝对值都是 1，rms 也是 1
        let (rms, peak) = level_of(&[1.0, -1.0, 1.0, -1.0]);
        assert!((rms - 1.0).abs() < 1e-6, "rms={rms}");
        assert!((peak - 1.0).abs() < 1e-6, "peak={peak}");

        // rms 是均方根而不是平均绝对值：[1,0] 应得 1/√2 ≈ 0.7071，不是 0.5
        let (rms, peak) = level_of(&[1.0, 0.0]);
        assert!((rms - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6, "rms={rms}");
        assert!((peak - 1.0).abs() < 1e-6);

        // 全静音
        let (rms, peak) = level_of(&[0.0; 8]);
        assert_eq!((rms, peak), (0.0, 0.0));
    }

    #[test]
    fn level_of_handles_empty_without_nan() {
        // 空输入不能返回 NaN——那会经 serde 变成 JSON 的 null 并让前端画出空洞。
        let (rms, peak) = level_of(&[]);
        assert!(rms.is_finite() && peak.is_finite());
        assert_eq!((rms, peak), (0.0, 0.0));
    }

    #[test]
    fn level_meter_is_inert_without_a_sink() {
        // 没接 sink 时（CLI 路径）不该有任何开销或 panic
        let mut m = LevelMeter::new(None, Source::Mic);
        m.push(&[1.0, -1.0]);
        m.tick_idle();
    }

    #[test]
    fn level_meter_emits_on_the_configured_interval() {
        use std::sync::atomic::AtomicUsize;

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let sink: LevelSink = Arc::new(move |s: LevelSample| {
            assert!(s.rms.is_finite() && s.peak.is_finite());
            h.fetch_add(1, Ordering::Relaxed);
        });

        let mut m = LevelMeter::new(Some(&sink), Source::System);
        // 刚建好就推，还没到间隔 → 不发
        m.push(&[0.5; 16]);
        assert_eq!(hits.load(Ordering::Relaxed), 0);

        std::thread::sleep(LEVEL_INTERVAL + std::time::Duration::from_millis(5));
        m.push(&[0.5; 16]);
        assert_eq!(hits.load(Ordering::Relaxed), 1, "过了间隔应该发一次");
    }

    #[test]
    fn level_metering_does_not_touch_the_silence_threshold_signal() {
        // 回归锁：静音判定读的是 downmix + 重采样**之后**的信号，
        // 电平表读的是之前的交织原始样本。硬声像立体声在两处的读数天然不同，
        // 谁要是图省事把两者并成一套，这个断言会先炸。
        let interleaved = [1.0f32, 0.0, 1.0, 0.0]; // 左满幅、右静音
        let (_, meter_peak) = level_of(&interleaved);
        let post = to_target_pcm(&interleaved, 2, TARGET_SAMPLE_RATE).unwrap();
        let silence_peak = post.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));

        assert!((meter_peak - 1.0).abs() < 1e-6, "电平表看到的是原始幅度");
        assert!((silence_peak - 0.5).abs() < 1e-6, "静音判定看到的是 downmix 后的幅度");
        assert!(meter_peak > silence_peak, "两套口径不可互换");
    }

    #[test]
    fn to_target_pcm_passes_mono_through() {
        let mono = [0.25f32, -0.25];
        let pcm = to_target_pcm(&mono, 1, TARGET_SAMPLE_RATE).unwrap();
        assert_eq!(pcm.samples, vec![0.25, -0.25]);
    }

    #[test]
    fn stop_signal_flips() {
        let s = StopSignal::new();
        assert!(!s.is_stopped());
        s.stop();
        assert!(s.is_stopped());
    }

    #[test]
    fn stop_signal_is_shared_across_clones() {
        let a = StopSignal::new();
        let b = a.clone();
        a.stop();
        assert!(b.is_stopped(), "克隆必须共享同一状态，否则停不掉子线程");
    }

    #[test]
    fn default_config_matches_plan() {
        let c = CaptureConfig::default();
        assert_eq!(c.chunk_seconds, 60, "§4.1 规定 60s 分片");
        assert!(c.record_mic && c.record_system, "默认双轨");
    }

    #[test]
    fn disabling_both_tracks_yields_empty_recording() {
        let cfg = CaptureConfig {
            record_mic: false,
            record_system: false,
            ..Default::default()
        };
        let stop = StopSignal::new();
        stop.stop();
        let rec = record_dual_track(&cfg, &stop).unwrap();
        assert!(rec.mic_chunks.is_empty() && rec.system_chunks.is_empty());
    }
}
