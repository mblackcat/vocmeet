//! 音频 I/O：WAV 读写、重采样、分片落盘。对应 §4.1。
//!
//! 全流程统一为 16kHz / 单声道 / f32（落盘时转 i16）。采集侧拿到的原始格式
//! 千奇百怪（44.1k/48k、立体声、f32/i16），统一在这里收敛。

use std::fs;
use std::path::{Path, PathBuf};

use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use crate::{Error, Result, TARGET_SAMPLE_RATE};

/// 一段单声道 16kHz PCM。
#[derive(Debug, Clone, Default)]
pub struct Pcm {
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn duration_ms(&self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        ((self.samples.len() as f64 / self.sample_rate as f64) * 1000.0) as u32
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// 读取 WAV 为单声道 f32。多声道自动混为单声道。
pub fn read_wav(path: impl AsRef<Path>) -> Result<Pcm> {
    let path = path.as_ref();
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| Error::audio(format!("open {}: {e}", path.display())))?;
    let spec = reader.spec();

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::audio(format!("read float samples: {e}")))?,
        hound::SampleFormat::Int => {
            // i16/i24/i32 统一按位深归一化到 [-1, 1]。
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| Error::audio(format!("read int samples: {e}")))?
        }
    };

    let samples = downmix(&interleaved, spec.channels as usize);
    Ok(Pcm {
        sample_rate: spec.sample_rate,
        samples,
    })
}

/// 写 16bit 单声道 WAV。
pub fn write_wav(path: impl AsRef<Path>, pcm: &Pcm) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: pcm.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| Error::audio(format!("create {}: {e}", path.display())))?;
    for &s in &pcm.samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer
            .write_sample(v)
            .map_err(|e| Error::audio(format!("write sample: {e}")))?;
    }
    writer
        .finalize()
        .map_err(|e| Error::audio(format!("finalize {}: {e}", path.display())))?;
    Ok(())
}

/// 交错多声道降为单声道（等权平均）。
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// 重采样到 [`TARGET_SAMPLE_RATE`]。已是目标采样率时零拷贝返回。
///
/// 设备原生采样率通常是 44.1k 或 48k，而全部模型都要求 16k——这一步是
/// §4.1「采样率不一致」那条现实问题的落地。
pub fn resample_to_target(pcm: &Pcm) -> Result<Pcm> {
    resample(pcm, TARGET_SAMPLE_RATE)
}

pub fn resample(pcm: &Pcm, target_rate: u32) -> Result<Pcm> {
    if pcm.sample_rate == target_rate || pcm.samples.is_empty() {
        return Ok(Pcm {
            sample_rate: target_rate,
            samples: pcm.samples.clone(),
        });
    }

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    let ratio = target_rate as f64 / pcm.sample_rate as f64;
    // 一次性处理整段：chunk 取输入长度，避免逐块调用时的边界处理。
    let chunk = pcm.samples.len();
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, chunk, 1)
        .map_err(|e| Error::audio(format!("create resampler: {e}")))?;

    let input = vec![pcm.samples.clone()];
    let output = resampler
        .process(&input, None)
        .map_err(|e| Error::audio(format!("resample: {e}")))?;

    Ok(Pcm {
        sample_rate: target_rate,
        samples: output.into_iter().next().unwrap_or_default(),
    })
}

/// 分片写入器：每 [`Self::chunk_seconds`] 秒落一个 WAV 分片。
///
/// 存在的理由见 §4.1：进程崩溃或断电时最多损失一个分片的时长，
/// 而不是整场会议。`seq` 单调递增，重启后按 seq 拼接即可。
pub struct ChunkWriter {
    dir: PathBuf,
    prefix: String,
    chunk_samples: usize,
    sample_rate: u32,
    buf: Vec<f32>,
    seq: u32,
    written: Vec<ChunkInfo>,
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub seq: u32,
    pub path: PathBuf,
    pub start_ms: u32,
    pub duration_ms: u32,
}

impl ChunkWriter {
    pub fn new(
        dir: impl Into<PathBuf>,
        prefix: impl Into<String>,
        sample_rate: u32,
        chunk_seconds: u32,
    ) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        Ok(Self {
            dir,
            prefix: prefix.into(),
            chunk_samples: (sample_rate as usize) * (chunk_seconds as usize),
            sample_rate,
            buf: Vec::with_capacity((sample_rate as usize) * (chunk_seconds as usize) + 4096),
            seq: 0,
            written: Vec::new(),
        })
    }

    /// 追加采样。内部满一片就落盘。
    pub fn push(&mut self, samples: &[f32]) -> Result<()> {
        self.buf.extend_from_slice(samples);
        while self.buf.len() >= self.chunk_samples {
            let rest = self.buf.split_off(self.chunk_samples);
            let chunk = std::mem::replace(&mut self.buf, rest);
            self.flush_chunk(chunk)?;
        }
        Ok(())
    }

    /// 落盘剩余不足一片的数据。停止录制时必须调用。
    pub fn finish(mut self) -> Result<Vec<ChunkInfo>> {
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            self.flush_chunk(chunk)?;
        }
        Ok(self.written)
    }

    fn flush_chunk(&mut self, samples: Vec<f32>) -> Result<()> {
        let seq = self.seq;
        let path = self.dir.join(format!("{}_{seq:05}.wav", self.prefix));
        let pcm = Pcm {
            sample_rate: self.sample_rate,
            samples,
        };
        let duration_ms = pcm.duration_ms();
        write_wav(&path, &pcm)?;
        self.written.push(ChunkInfo {
            seq,
            path,
            start_ms: (seq as u64 * (self.chunk_samples as u64) * 1000
                / self.sample_rate as u64) as u32,
            duration_ms,
        });
        self.seq += 1;
        Ok(())
    }

    pub fn chunks(&self) -> &[ChunkInfo] {
        &self.written
    }
}


/// 扫到一个目录里某条轨的全部分片，按 seq 排好。
///
/// 录制结束后分片就一直躺在会议目录里（`mic_00000.wav` / `sys_00000.wav`），
/// 重新解析时不必回头去问内存里的状态——那份状态早随进程没了。
///
/// `start_ms` 用累加实际时长算，而不是 `seq × 分片长度`：最后一片通常不满，
/// 中间要是缺了一片，累加至少不会把后面所有时间戳整体推错。
pub fn scan_chunks(dir: impl AsRef<Path>, prefix: &str) -> Result<Vec<ChunkInfo>> {
    let dir = dir.as_ref();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wav") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(seq) = stem
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('_'))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        found.push((seq, path));
    }
    found.sort_by_key(|(seq, _)| *seq);

    let mut out = Vec::with_capacity(found.len());
    let mut cursor_ms: u64 = 0;
    for (seq, path) in found {
        // 只读头，不读采样——挑分片不该把整场会议搬进内存。
        let reader = hound::WavReader::open(&path)
            .map_err(|e| Error::audio(format!("open {}: {e}", path.display())))?;
        let spec = reader.spec();
        let frames = reader.duration() as u64;
        let duration_ms = if spec.sample_rate == 0 {
            0
        } else {
            frames * 1000 / spec.sample_rate as u64
        };
        out.push(ChunkInfo {
            seq,
            path,
            start_ms: cursor_ms as u32,
            duration_ms: duration_ms as u32,
        });
        cursor_ms += duration_ms;
    }
    Ok(out)
}

/// 把双轨混成一条可回放的音轨。
///
/// 转写用的是分开的两轨（麦克风轨不做 diarization），但回放时用户要听到完整现场，
/// 所以这里做等权相加并 clamp。两轨长度不等时以较长者为准，短的那条补静音。
pub fn mix_tracks(a: &Pcm, b: &Pcm) -> Result<Pcm> {
    if a.samples.is_empty() {
        return Ok(b.clone());
    }
    if b.samples.is_empty() {
        return Ok(a.clone());
    }
    if a.sample_rate != b.sample_rate {
        return Err(Error::audio(format!(
            "mix: sample rate mismatch {} vs {}",
            a.sample_rate, b.sample_rate
        )));
    }
    let n = a.samples.len().max(b.samples.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let x = a.samples.get(i).copied().unwrap_or(0.0);
        let y = b.samples.get(i).copied().unwrap_or(0.0);
        out.push((x + y).clamp(-1.0, 1.0));
    }
    Ok(Pcm {
        sample_rate: a.sample_rate,
        samples: out,
    })
}

/// 按 seq 顺序把分片拼回整轨。转写前调用。
pub fn concat_chunks(chunks: &[ChunkInfo]) -> Result<Pcm> {
    let mut ordered: Vec<&ChunkInfo> = chunks.iter().collect();
    ordered.sort_by_key(|c| c.seq);

    let mut out = Pcm {
        sample_rate: TARGET_SAMPLE_RATE,
        samples: Vec::new(),
    };
    for c in ordered {
        let pcm = read_wav(&c.path)?;
        if out.samples.is_empty() {
            out.sample_rate = pcm.sample_rate;
        } else if pcm.sample_rate != out.sample_rate {
            return Err(Error::audio(format!(
                "chunk {} sample rate {} != {}",
                c.seq, pcm.sample_rate, out.sample_rate
            )));
        }
        out.samples.extend_from_slice(&pcm.samples);
    }
    Ok(out)
}


// ---------------------------------------------------------------- 导入外部音频（§4.1）

/// 能导入的扩展名。这份清单与 `symphonia` 打开的 feature 一一对应——
/// 列出来却解不了，用户看到的是「导入失败」而不是「不支持这种格式」，差别很大。
pub const IMPORTABLE_EXTENSIONS: &[&str] = &[
    "wav", "wave", "aac", "m4a", "m4b", "mp4", "mp3", "flac", "ogg", "oga", "mka", "webm",
    "aiff", "aif", "aifc", "caf",
];

/// 扩展名是否在可导入清单里。大小写不敏感。
pub fn is_importable(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMPORTABLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 解码进度。`total_ms` 为 `None` 表示容器里没写时长——裸 ADTS 的 `.aac` 就是这样，
/// 这时只能报「已解码多少」，报不了百分比。
#[derive(Debug, Clone, Copy)]
pub struct DecodeProgress {
    pub decoded_ms: u64,
    pub total_ms: Option<u64>,
}

impl DecodeProgress {
    /// 已知总时长时的完成比例。未知返回 `None`，UI 据此决定画进度条还是画计数。
    pub fn fraction(&self) -> Option<f32> {
        match self.total_ms {
            Some(total) if total > 0 => {
                Some((self.decoded_ms as f32 / total as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}

/// 增量 WAV 写入器：给导入用，边解边写。
///
/// [`write_wav`] 要求整段 PCM 先在内存里凑齐，而导入的典型输入就是一两小时的录音，
/// 48k 立体声解出来是 GB 级——那条路走不通。
pub struct WavSink {
    writer: hound::WavWriter<std::io::BufWriter<fs::File>>,
    sample_rate: u32,
    frames: u64,
}

impl WavSink {
    pub fn create(path: impl AsRef<Path>, sample_rate: u32) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec)
            .map_err(|e| Error::audio(format!("create {}: {e}", path.display())))?;
        Ok(Self {
            writer,
            sample_rate,
            frames: 0,
        })
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            self.writer
                .write_sample(v)
                .map_err(|e| Error::audio(format!("write sample: {e}")))?;
        }
        self.frames += samples.len() as u64;
        Ok(())
    }

    pub fn duration_ms(&self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        (self.frames * 1000 / self.sample_rate as u64) as u32
    }

    /// 收尾并回填 WAV 头。返回写入的时长（毫秒）。
    pub fn finish(self) -> Result<u32> {
        let ms = self.duration_ms();
        self.writer
            .finalize()
            .map_err(|e| Error::audio(format!("finalize wav: {e}")))?;
        Ok(ms)
    }
}

/// 流式重采样器：固定输入块长喂进去，边喂边出。
///
/// 与 [`resample`] 的区别只在内存——那个函数把 chunk 设成整段长度，一次性处理；
/// 这个把 rubato 的同一个实例跨块复用，滤波器状态是连续的，所以块边界不会有断点。
struct StreamResampler {
    /// `None` 表示采样率本来就对，直通不做任何处理。
    inner: Option<SincFixedIn<f32>>,
    pending: Vec<f32>,
    chunk: usize,
    ratio: f64,
}

impl StreamResampler {
    fn new(from_rate: u32, to_rate: u32) -> Result<Self> {
        if from_rate == to_rate || from_rate == 0 {
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
                chunk: 0,
                ratio: 1.0,
            });
        }
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = to_rate as f64 / from_rate as f64;
        // 一秒一块：再小，sinc 的边界开销占比就上来了；再大，省内存的意义就没了。
        let chunk = from_rate as usize;
        let inner = SincFixedIn::<f32>::new(ratio, 2.0, params, chunk, 1)
            .map_err(|e| Error::audio(format!("create resampler: {e}")))?;
        Ok(Self {
            inner: Some(inner),
            pending: Vec::with_capacity(chunk * 2),
            chunk,
            ratio,
        })
    }

    /// 送入一段原始采样。攒满一块就重采样一块，结果交给 `out`。
    fn push(&mut self, samples: &[f32], out: &mut dyn FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let Some(resampler) = self.inner.as_mut() else {
            return out(samples);
        };
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= self.chunk {
            let rest = self.pending.split_off(self.chunk);
            let block = std::mem::replace(&mut self.pending, rest);
            let done = resampler
                .process(&[block], None)
                .map_err(|e| Error::audio(format!("resample: {e}")))?;
            out(&done[0])?;
        }
        Ok(())
    }

    /// 冲掉不足一块的尾巴：补零凑满送进去，再按比例把补出来的那截截掉。
    fn finish(mut self, out: &mut dyn FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let tail = std::mem::take(&mut self.pending);
        let chunk = self.chunk;
        let ratio = self.ratio;
        let Some(resampler) = self.inner.as_mut() else {
            return if tail.is_empty() { Ok(()) } else { out(&tail) };
        };
        if tail.is_empty() {
            return Ok(());
        }
        let valid = tail.len();
        let mut block = tail;
        block.resize(chunk, 0.0);
        let done = resampler
            .process(&[block], None)
            .map_err(|e| Error::audio(format!("resample tail: {e}")))?;
        let keep = ((valid as f64) * ratio).round() as usize;
        let n = keep.min(done[0].len());
        out(&done[0][..n])
    }
}

/// 把外部音频文件解码成 16kHz 单声道 WAV，边解码边落盘。返回时长（毫秒）。
///
/// 全流程只认 16k 单声道（见 [`crate::TARGET_SAMPLE_RATE`]），所以导入必须过这一道；
/// 顺带也解决了「webview 播不了 .aac」——落盘的永远是 WAV，回放与转写共用同一个文件。
///
/// 单个坏包会被跳过而不是让整场导入失败：一小时录音里坏几帧很常见，
/// 为此丢掉整场不划算。
pub fn transcode_to_wav(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    on_progress: &dyn Fn(DecodeProgress),
) -> Result<u32> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    transcode_inner(src, dst, on_progress).inspect_err(|_| {
        // 半截的 WAV 比没有更糟：它会被当成一份能用的音轨。
        let _ = fs::remove_file(dst);
    })
}

fn transcode_inner(src: &Path, dst: &Path, on_progress: &dyn Fn(DecodeProgress)) -> Result<u32> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::TrackType;
    use symphonia::core::io::MediaSourceStream;

    let file = fs::File::open(src).map_err(|e| Error::io(src, e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut reader = symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .map_err(|e| Error::audio(format!("认不出 {} 的音频格式：{e}", src.display())))?;

    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| Error::audio(format!("{} 里没有音频轨", src.display())))?;
    let track_id = track.id;
    let num_frames = track.num_frames;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => {
            return Err(Error::audio(format!(
                "{} 的音频轨缺少编码参数，无法解码",
                src.display()
            )))
        }
    };
    // 容器声明的时长。裸 ADTS 没有，那进度就只能是「已解码 X」。
    let total_ms = match (num_frames, params.sample_rate) {
        (Some(frames), Some(rate)) if rate > 0 => Some(frames * 1000 / rate as u64),
        _ => None,
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &Default::default())
        .map_err(|e| Error::audio(format!("{} 用的编码格式暂不支持：{e}", src.display())))?;

    let mut sink = WavSink::create(dst, TARGET_SAMPLE_RATE)?;
    let mut resampler: Option<StreamResampler> = None;
    let mut source_rate = 0u32;
    let mut interleaved: Vec<f32> = Vec::new();
    let mut in_frames: u64 = 0;
    let mut reported_ms: u64 = 0;
    let mut bad_packets: u64 = 0;

    loop {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Error::audio(format!("读取音频包失败：{e}"))),
        };
        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => {
                bad_packets += 1;
                continue;
            }
            Err(e) => return Err(Error::audio(format!("解码失败：{e}"))),
        };
        if decoded.is_empty() {
            continue;
        }

        let rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count().max(1);
        decoded.copy_to_vec_interleaved(&mut interleaved);
        let mono = downmix(&interleaved, channels);

        if source_rate == 0 {
            source_rate = rate;
            resampler = Some(StreamResampler::new(rate, TARGET_SAMPLE_RATE)?);
        }
        let Some(r) = resampler.as_mut() else {
            unreachable!("resampler 在见到第一个包时就已建好")
        };
        r.push(&mono, &mut |out| sink.write(out))?;

        in_frames += mono.len() as u64;
        let decoded_ms = in_frames * 1000 / source_rate.max(1) as u64;
        // 每两秒音频报一次，别把事件通道刷爆。
        if decoded_ms >= reported_ms + 2_000 {
            reported_ms = decoded_ms;
            on_progress(DecodeProgress {
                decoded_ms,
                total_ms,
            });
        }
    }

    if let Some(r) = resampler {
        r.finish(&mut |out| sink.write(out))?;
    }
    if bad_packets > 0 {
        tracing::warn!("导入 {} 时跳过了 {bad_packets} 个坏包", src.display());
    }

    let duration_ms = sink.finish()?;
    if duration_ms == 0 {
        return Err(Error::audio(format!("{} 没有解出任何音频", src.display())));
    }
    on_progress(DecodeProgress {
        decoded_ms: in_frames * 1000 / source_rate.max(1) as u64,
        total_ms,
    });
    Ok(duration_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_channels() {
        let stereo = vec![1.0, 0.0, 0.5, 0.5];
        assert_eq!(downmix(&stereo, 2), vec![0.5, 0.5]);
        assert_eq!(downmix(&stereo, 1), stereo);
    }

    #[test]
    fn resample_is_noop_at_target_rate() {
        let pcm = Pcm {
            sample_rate: TARGET_SAMPLE_RATE,
            samples: vec![0.1, 0.2, 0.3],
        };
        let out = resample_to_target(&pcm).unwrap();
        assert_eq!(out.samples, pcm.samples);
    }

    #[test]
    fn resample_changes_length_proportionally() {
        // 48k -> 16k 应约为三分之一长度。
        let pcm = Pcm {
            sample_rate: 48_000,
            samples: vec![0.0; 48_000],
        };
        let out = resample_to_target(&pcm).unwrap();
        assert_eq!(out.sample_rate, TARGET_SAMPLE_RATE);
        let ratio = out.samples.len() as f64 / 16_000.0;
        assert!(ratio > 0.9 && ratio < 1.1, "unexpected length {}", out.samples.len());
    }

    #[test]
    fn mix_tracks_sums_and_clamps() {
        let a = Pcm { sample_rate: 16_000, samples: vec![0.5, 0.9, 0.0] };
        let b = Pcm { sample_rate: 16_000, samples: vec![0.5, 0.9] };
        let m = mix_tracks(&a, &b).unwrap();
        assert_eq!(m.samples.len(), 3, "以较长的一轨为准");
        assert!((m.samples[0] - 1.0).abs() < 1e-6);
        assert!((m.samples[1] - 1.0).abs() < 1e-6, "溢出必须 clamp 到 1.0");
        assert_eq!(m.samples[2], 0.0, "短轨补静音");
    }

    #[test]
    fn mix_tracks_handles_empty_side() {
        let a = Pcm { sample_rate: 16_000, samples: vec![0.3] };
        let empty = Pcm { sample_rate: 16_000, samples: vec![] };
        assert_eq!(mix_tracks(&a, &empty).unwrap().samples, vec![0.3]);
        assert_eq!(mix_tracks(&empty, &a).unwrap().samples, vec![0.3]);
    }

    #[test]
    fn chunk_writer_splits_and_concats_roundtrip() {
        let dir = std::env::temp_dir().join(format!("vocmeet_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sr = 16_000u32;
        let mut w = ChunkWriter::new(&dir, "mic", sr, 1).unwrap();
        // 2.5 秒 -> 3 个分片（1s, 1s, 0.5s）
        let samples = vec![0.25f32; (sr as f32 * 2.5) as usize];
        w.push(&samples).unwrap();
        let chunks = w.finish().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].start_ms, 1000);

        let joined = concat_chunks(&chunks).unwrap();
        assert_eq!(joined.samples.len(), samples.len());
        let _ = fs::remove_dir_all(&dir);
    }


    #[test]
    fn scan_chunks_orders_by_seq_and_accumulates_start() {
        let dir = std::env::temp_dir().join(format!("vocmeet_scan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sr = TARGET_SAMPLE_RATE;

        // 故意乱序写、故意让最后一片不满。
        for (seq, secs) in [(1u32, 1.0f32), (0, 2.0), (2, 0.5)] {
            let pcm = Pcm {
                sample_rate: sr,
                samples: vec![0.1; (sr as f32 * secs) as usize],
            };
            write_wav(dir.join(format!("sys_{seq:05}.wav")), &pcm).unwrap();
        }
        // 另一条轨与无关文件都不该被扫进来。
        write_wav(
            dir.join("mic_00000.wav"),
            &Pcm { sample_rate: sr, samples: vec![0.0; sr as usize] },
        )
        .unwrap();
        fs::write(dir.join("playback.wav.txt"), b"noise").unwrap();

        let chunks = scan_chunks(&dir, "sys").unwrap();
        assert_eq!(chunks.len(), 3, "只该扫到 sys 那三片");
        assert_eq!(chunks.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(chunks[0].start_ms, 0);
        assert_eq!(chunks[1].start_ms, 2000, "start 是累加出来的，不是 seq 乘出来的");
        assert_eq!(chunks[2].start_ms, 3000);
        assert_eq!(chunks[2].duration_ms, 500, "最后一片不满也要如实报");

        assert_eq!(scan_chunks(&dir, "mic").unwrap().len(), 1);
        assert!(scan_chunks(dir.join("nope"), "sys").unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn importable_matches_extensions_case_insensitively() {
        assert!(is_importable("a/b/会议.aac"));
        assert!(is_importable("REC.M4A"), "扩展名大小写不该影响判断");
        assert!(is_importable("x.wav"));
        assert!(!is_importable("notes.txt"));
        assert!(!is_importable("no_extension"));
    }

    #[test]
    fn decode_progress_fraction_needs_a_known_total() {
        let known = DecodeProgress { decoded_ms: 500, total_ms: Some(1000) };
        assert_eq!(known.fraction(), Some(0.5));
        // 裸 ADTS 没有时长，只能报计数。
        let unknown = DecodeProgress { decoded_ms: 500, total_ms: None };
        assert_eq!(unknown.fraction(), None);
        let zero = DecodeProgress { decoded_ms: 0, total_ms: Some(0) };
        assert_eq!(zero.fraction(), None);
    }

    #[test]
    fn wav_sink_writes_incrementally() {
        let dir = std::env::temp_dir().join(format!("vocmeet_sink_{}", std::process::id()));
        let path = dir.join("out.wav");
        let _ = fs::remove_dir_all(&dir);

        let mut sink = WavSink::create(&path, TARGET_SAMPLE_RATE).unwrap();
        sink.write(&[0.5f32; 8_000]).unwrap();
        sink.write(&[-0.5f32; 8_000]).unwrap();
        assert_eq!(sink.duration_ms(), 1_000);
        assert_eq!(sink.finish().unwrap(), 1_000);

        let back = read_wav(&path).unwrap();
        assert_eq!(back.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(back.samples.len(), 16_000);
        assert!((back.samples[0] - 0.5).abs() < 1e-3);
        assert!((back.samples[15_999] + 0.5).abs() < 1e-3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_resampler_matches_target_rate_across_blocks() {
        // 48k 的 3 秒送进去，出来应该约等于 16k 的 3 秒。
        let mut r = StreamResampler::new(48_000, TARGET_SAMPLE_RATE).unwrap();
        let mut out: Vec<f32> = Vec::new();
        let block = vec![0.1f32; 7_000]; // 故意不对齐块长，逼出内部缓冲
        for _ in 0..20 {
            r.push(&block, &mut |o| {
                out.extend_from_slice(o);
                Ok(())
            })
            .unwrap();
        }
        r.finish(&mut |o| {
            out.extend_from_slice(o);
            Ok(())
        })
        .unwrap();

        let expected = 140_000f64 / 3.0; // 48k -> 16k
        let ratio = out.len() as f64 / expected;
        assert!(ratio > 0.97 && ratio < 1.03, "长度偏差过大: {}", out.len());
    }

    #[test]
    fn stream_resampler_passes_through_at_same_rate() {
        let mut r = StreamResampler::new(TARGET_SAMPLE_RATE, TARGET_SAMPLE_RATE).unwrap();
        let mut out: Vec<f32> = Vec::new();
        r.push(&[0.1, 0.2, 0.3], &mut |o| {
            out.extend_from_slice(o);
            Ok(())
        })
        .unwrap();
        r.finish(&mut |o| {
            out.extend_from_slice(o);
            Ok(())
        })
        .unwrap();
        assert_eq!(out, vec![0.1, 0.2, 0.3], "同采样率不该动样本");
    }

    #[test]
    fn transcode_downsamples_and_reports_progress() {
        let dir = std::env::temp_dir().join(format!("vocmeet_import_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let src = dir.join("src.wav");
        let dst = dir.join("dst.wav");

        // 44.1kHz 的 4 秒正弦，走 symphonia 的 wav reader 把整条链路跑一遍。
        let rate = 44_100u32;
        let samples: Vec<f32> = (0..rate * 4)
            .map(|i| (i as f32 * 0.05).sin() * 0.4)
            .collect();
        write_wav(&src, &Pcm { sample_rate: rate, samples }).unwrap();

        let seen = std::cell::RefCell::new(Vec::new());
        let ms = transcode_to_wav(&src, &dst, &|p| seen.borrow_mut().push(p)).unwrap();

        assert!((ms as i64 - 4_000).abs() < 100, "时长应约 4 秒，实际 {ms}ms");
        let back = read_wav(&dst).unwrap();
        assert_eq!(back.sample_rate, TARGET_SAMPLE_RATE, "落盘必须是 16k");
        let ratio = back.samples.len() as f64 / 64_000.0;
        assert!(ratio > 0.97 && ratio < 1.03, "样本数不对: {}", back.samples.len());

        let reports = seen.borrow();
        assert!(!reports.is_empty(), "应当报过进度");
        // WAV 头里有帧数，所以百分比是算得出来的。
        assert!(reports.last().unwrap().fraction().is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcode_rejects_a_file_that_is_not_audio() {
        let dir = std::env::temp_dir().join(format!("vocmeet_bad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("fake.wav");
        let dst = dir.join("out.wav");
        fs::write(&src, b"this is definitely not a wav file").unwrap();

        assert!(transcode_to_wav(&src, &dst, &|_| {}).is_err());
        assert!(!dst.exists(), "失败时不该留下半截 WAV");
        let _ = fs::remove_dir_all(&dir);
    }
}
