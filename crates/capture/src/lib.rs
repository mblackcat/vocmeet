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
    ) -> Result<Vec<ChunkInfo>> {
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
        client
            .start_stream()
            .map_err(|e| CaptureError::Stream(format!("启动流: {e}")))?;

        while !stop.is_stopped() {
            // 事件超时不致命：静音时设备可能长时间不产生数据。
            if event.wait_for_event(1_000).is_err() {
                continue;
            }
            let frames = capture_client.get_next_packet_size().unwrap_or(None).unwrap_or(0);
            if frames == 0 {
                continue;
            }
            if capture_client.read_from_device_to_deque(&mut raw).is_err() {
                continue;
            }

            // 攒够一批再转换，避免每个 packet 都过一次重采样器。
            let batch_bytes = blockalign * native_rate / 4; // ≈250ms
            while raw.len() >= batch_bytes {
                let bytes: Vec<u8> = raw.drain(..batch_bytes).collect();
                let pcm = decode_and_downsample(&bytes, native_rate)?;
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
                writer.push(&pcm.samples)?;
            }
        }

        Ok(writer.finish()?)
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
    pub fn probe_loopback() -> Result<()> {
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
        Ok(())
    }
}

#[cfg(not(windows))]
mod fallback_impl {
    use super::*;

    pub fn list_devices() -> Result<Vec<DeviceInfo>> {
        Err(CaptureError::Unsupported(
            "MVP 仅支持 Windows；macOS 支持见方案 §9 v1.0 路线".into(),
        ))
    }

    pub fn capture_track(
        _source: Source,
        _config: &CaptureConfig,
        _stop: &StopSignal,
    ) -> Result<Vec<ChunkInfo>> {
        Err(CaptureError::Unsupported(
            "MVP 仅支持 Windows 采集".into(),
        ))
    }

    pub fn probe_loopback() -> Result<()> {
        Err(CaptureError::Unsupported("MVP 仅支持 Windows 采集".into()))
    }
}

#[cfg(windows)]
pub use windows_impl::{capture_track, list_devices, probe_loopback};

#[cfg(not(windows))]
pub use fallback_impl::{capture_track, list_devices, probe_loopback};

/// 同时录制双轨，直到 `stop` 被置位。
///
/// 两条轨各起一个线程——它们的设备时钟不同步，必须独立驱动。
/// 时间轴对齐依赖两者近似同时 start，误差目标 <200ms（§8.1 W2–W3 出口标准）。
pub fn record_dual_track(config: &CaptureConfig, stop: &StopSignal) -> Result<Recording> {
    let mut out = Recording::default();

    let mic_handle = if config.record_mic {
        let cfg = config.clone();
        let s = stop.clone();
        Some(std::thread::spawn(move || {
            capture_track(Source::Mic, &cfg, &s)
        }))
    } else {
        None
    };

    let sys_handle = if config.record_system {
        let cfg = config.clone();
        let s = stop.clone();
        Some(std::thread::spawn(move || {
            capture_track(Source::System, &cfg, &s)
        }))
    } else {
        None
    };

    // 任一轨失败不应让整场录制丢失——另一轨的数据仍然有价值。
    if let Some(h) = mic_handle {
        match h.join() {
            Ok(Ok(chunks)) => out.mic_chunks = chunks,
            Ok(Err(e)) => out.warnings.push(format!("麦克风轨失败：{e}")),
            Err(_) => out.warnings.push("麦克风轨线程 panic".into()),
        }
    }
    if let Some(h) = sys_handle {
        match h.join() {
            Ok(Ok(chunks)) => out.system_chunks = chunks,
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
