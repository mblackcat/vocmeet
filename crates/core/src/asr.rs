//! 转写引擎：VAD → ASR → diarization → 标点恢复。对应 §4.2。
//!
//! 设计要点（与 `docs/vocmeet_mvp_plan.md` 一致）：
//! - ASR 与 diarization **解耦**，两条结果在 [`crate::align`] 里按时间轴对齐；
//! - 麦克风轨不做 diarization，说话人恒为本机用户（双轨录制策略 §4.1）；
//! - 引擎藏在 [`TranscriptionEngine`] trait 后面，便于 W1 bake-off 期间换模型。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sherpa_onnx::{
    FastClusteringConfig, OfflinePunctuation, OfflinePunctuationConfig,
    OfflinePunctuationModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineSenseVoiceModelConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SileroVadModelConfig, SpeakerEmbeddingExtractorConfig, VadModelConfig, VoiceActivityDetector,
};

use crate::align::{self, DiarSegment, SpeechSegment};
use crate::audio::Pcm;
use crate::models::ModelSet;
use crate::{Error, Result, Source, Utterance, LOCAL_SPEAKER_ID, TARGET_SAMPLE_RATE};

/// 进度阶段。UI 用它渲染「阶段 + 百分比」，见 §6。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    LoadingModels,
    Vad,
    Asr,
    Diarization,
    Punctuation,
    Done,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub stage: Stage,
    /// 0.0 ~ 1.0
    pub fraction: f32,
    pub detail: String,
}

/// 取消令牌。长任务每处理完一段就检查一次。
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// 一次转写任务的输入：双轨 PCM。
pub struct TranscribeJob {
    /// 麦克风轨（本机用户）。可为 None——只录了系统音的场景。
    pub mic: Option<Pcm>,
    /// 系统回环轨（远端参会者）。可为 None——纯本地口述的场景。
    pub system: Option<Pcm>,
    /// 已知参会人数。`Some(n)` 走 num_clusters，`None` 走阈值自动估计（§4.2）。
    pub known_speakers: Option<i32>,
}

/// 转写引擎抽象。刻意保留这层，是为了 W1 bake-off 换模型时上层零改动，
/// 同时也是 §8.2 许可证风险的兜底（换一条许可证清洁的 ASR 链路即可）。
pub trait TranscriptionEngine: Send + Sync {
    fn transcribe(
        &self,
        job: &TranscribeJob,
        progress: &dyn Fn(Progress),
        cancel: &CancelToken,
    ) -> Result<Vec<Utterance>>;
}

/// 引擎可调参数。默认值来自 sherpa-onnx 官方示例，W1 调参后落到配置文件。
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub num_threads: i32,
    pub provider: String,
    /// VAD 单段最长秒数。过长会让 ASR 单次输入过大，过短会切碎句子。
    pub max_speech_duration: f32,
    pub vad_threshold: f32,
    pub min_silence_duration: f32,
    pub min_speech_duration: f32,
    /// 未知人数时聚类阈值。§7 的「说话人数估计准确率」就是为标定它。
    pub cluster_threshold: f32,
    /// 相邻同说话人段落合并的最大间隔（毫秒）。§4.2：仅 <2s 才合并。
    pub merge_gap_ms: u32,
    pub debug: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            num_threads: 2,
            provider: "cpu".to_string(),
            max_speech_duration: 20.0,
            vad_threshold: 0.5,
            min_silence_duration: 0.25,
            min_speech_duration: 0.25,
            cluster_threshold: 0.5,
            merge_gap_ms: 2_000,
            debug: false,
        }
    }
}

/// 基于 sherpa-onnx 的实现。
pub struct SherpaEngine {
    recognizer: OfflineRecognizer,
    punctuation: Option<OfflinePunctuation>,
    vad_config: VadModelConfig,
    segmentation_model: String,
    embedding_model: String,
    opts: EngineOptions,
}

impl SherpaEngine {
    pub fn new(models: &ModelSet, opts: EngineOptions) -> Result<Self> {
        models.verify_present()?;

        let mut rec_config = OfflineRecognizerConfig::default();
        rec_config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(path_str(&models.asr_model)?),
            language: Some("auto".to_string()),
            // 逆文本正则化：把「二零二六年」转成「2026年」，纪要可读性提升明显。
            use_itn: true,
        };
        rec_config.model_config.tokens = Some(path_str(&models.asr_tokens)?);
        rec_config.model_config.provider = Some(opts.provider.clone());
        rec_config.model_config.num_threads = opts.num_threads;
        rec_config.model_config.debug = opts.debug;

        let recognizer = OfflineRecognizer::create(&rec_config)
            .ok_or_else(|| Error::engine("failed to create OfflineRecognizer (检查 ASR 模型与 tokens 是否匹配)"))?;

        let punctuation = OfflinePunctuation::create(&OfflinePunctuationConfig {
            model: OfflinePunctuationModelConfig {
                ct_transformer: Some(path_str(&models.punctuation_model)?),
                num_threads: opts.num_threads,
                provider: Some(opts.provider.clone()),
                debug: opts.debug,
            },
        });
        if punctuation.is_none() {
            // 标点恢复失败不致命：逐字稿仍可用，只是可读性下降。
            tracing::warn!("标点模型加载失败，将输出无标点逐字稿");
        }

        let mut silero = SileroVadModelConfig::default();
        silero.model = Some(path_str(&models.vad_model)?);
        silero.threshold = opts.vad_threshold;
        silero.min_silence_duration = opts.min_silence_duration;
        silero.min_speech_duration = opts.min_speech_duration;
        silero.max_speech_duration = opts.max_speech_duration;

        let vad_config = VadModelConfig {
            silero_vad: silero,
            ten_vad: Default::default(),
            sample_rate: TARGET_SAMPLE_RATE as i32,
            num_threads: 1,
            provider: Some(opts.provider.clone()),
            debug: opts.debug,
        };

        Ok(Self {
            recognizer,
            punctuation,
            vad_config,
            segmentation_model: path_str(&models.segmentation_model)?,
            embedding_model: path_str(&models.embedding_model)?,
            opts,
        })
    }

    /// VAD 切分出语音段。返回的时间戳以整轨起点为基准。
    fn split_speech(&self, pcm: &Pcm, cancel: &CancelToken) -> Result<Vec<SpeechSegment>> {
        let vad = VoiceActivityDetector::create(&self.vad_config, 30.0)
            .ok_or_else(|| Error::engine("failed to create VoiceActivityDetector"))?;

        let mut segments = Vec::new();
        // sherpa 的 silero 前端要求 512 采样窗口。
        const WINDOW: usize = 512;

        let drain = |vad: &VoiceActivityDetector, out: &mut Vec<SpeechSegment>| {
            while let Some(seg) = vad.front() {
                let start = seg.start().max(0) as usize;
                let samples = seg.samples().to_vec();
                if !samples.is_empty() {
                    out.push(SpeechSegment {
                        start_ms: samples_to_ms(start),
                        end_ms: samples_to_ms(start + samples.len()),
                        samples,
                    });
                }
                vad.pop();
            }
        };

        for (i, chunk) in pcm.samples.chunks(WINDOW).enumerate() {
            if i % 256 == 0 {
                cancel.check()?;
            }
            vad.accept_waveform(chunk);
            drain(&vad, &mut segments);
        }
        vad.flush();
        drain(&vad, &mut segments);

        Ok(segments)
    }

    /// 对每个语音段跑 ASR。空结果直接丢弃（VAD 误检的静音段）。
    fn recognize_segments(
        &self,
        segments: &[SpeechSegment],
        source: Source,
        progress: &dyn Fn(Progress),
        cancel: &CancelToken,
    ) -> Result<Vec<align::AsrSegment>> {
        let mut out = Vec::with_capacity(segments.len());
        let total = segments.len().max(1);

        for (i, seg) in segments.iter().enumerate() {
            cancel.check()?;

            let stream = self.recognizer.create_stream();
            stream.accept_waveform(TARGET_SAMPLE_RATE as i32, &seg.samples);
            self.recognizer.decode(&stream);

            if let Some(result) = stream.get_result() {
                let text = result.text.trim().to_string();
                if !text.is_empty() {
                    out.push(align::AsrSegment {
                        start_ms: seg.start_ms,
                        end_ms: seg.end_ms,
                        text,
                        // 词级时间戳以段起点为基准偏移到整轨坐标。
                        word_times_ms: result.timestamps.map(|ts| {
                            ts.iter()
                                .map(|t| seg.start_ms + (t * 1000.0) as u32)
                                .collect()
                        }),
                        source,
                    });
                }
            }

            progress(Progress {
                stage: Stage::Asr,
                fraction: (i + 1) as f32 / total as f32,
                detail: format!("{}/{} 段", i + 1, total),
            });
        }
        Ok(out)
    }

    /// 系统轨的说话人分离。
    fn diarize(&self, pcm: &Pcm, known_speakers: Option<i32>) -> Result<Vec<DiarSegment>> {
        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(self.segmentation_model.clone()),
                    window_shift_ratio: 0.1,
                },
                ..Default::default()
            },
            embedding: SpeakerEmbeddingExtractorConfig {
                model: Some(self.embedding_model.clone()),
                ..Default::default()
            },
            clustering: FastClusteringConfig {
                // 已知人数用 num_clusters（更准）；未知传 -1 走阈值。见 §4.2。
                num_clusters: known_speakers.unwrap_or(-1),
                threshold: self.opts.cluster_threshold,
            },
            ..Default::default()
        };

        let sd = OfflineSpeakerDiarization::create(&config)
            .ok_or_else(|| Error::engine("failed to create OfflineSpeakerDiarization"))?;

        let result = sd
            .process(&pcm.samples)
            .ok_or_else(|| Error::engine("speaker diarization failed"))?;

        Ok(result
            .sort_by_start_time()
            .into_iter()
            .map(|s| DiarSegment {
                start_ms: (s.start * 1000.0) as u32,
                end_ms: (s.end * 1000.0) as u32,
                speaker: s.speaker,
            })
            .collect())
    }

    fn punctuate(&self, text: &str) -> String {
        match &self.punctuation {
            Some(p) => p.add_punctuation(text).unwrap_or_else(|| text.to_string()),
            None => text.to_string(),
        }
    }
}

impl TranscriptionEngine for SherpaEngine {
    fn transcribe(
        &self,
        job: &TranscribeJob,
        progress: &dyn Fn(Progress),
        cancel: &CancelToken,
    ) -> Result<Vec<Utterance>> {
        let mut asr_segments: Vec<align::AsrSegment> = Vec::new();
        let mut diar_segments: Vec<DiarSegment> = Vec::new();

        // ---- 系统轨：VAD → ASR，同时独立跑 diarization ----
        if let Some(sys) = &job.system {
            require_target_rate(sys)?;

            progress(Progress {
                stage: Stage::Vad,
                fraction: 0.0,
                detail: "系统轨切分".into(),
            });
            let speech = self.split_speech(sys, cancel)?;

            asr_segments.extend(self.recognize_segments(&speech, Source::System, progress, cancel)?);

            cancel.check()?;
            progress(Progress {
                stage: Stage::Diarization,
                fraction: 0.0,
                detail: "说话人分离".into(),
            });
            diar_segments = self.diarize(sys, job.known_speakers)?;
            progress(Progress {
                stage: Stage::Diarization,
                fraction: 1.0,
                detail: format!("{} 段", diar_segments.len()),
            });
        }

        // ---- 麦克风轨：只做 VAD + ASR，说话人固定 ----
        if let Some(mic) = &job.mic {
            require_target_rate(mic)?;

            progress(Progress {
                stage: Stage::Vad,
                fraction: 0.0,
                detail: "麦克风轨切分".into(),
            });
            let speech = self.split_speech(mic, cancel)?;
            asr_segments.extend(self.recognize_segments(&speech, Source::Mic, progress, cancel)?);
        }

        cancel.check()?;

        // ---- 对齐、排序、合并 ----
        let mut utterances = align::assign_speakers(asr_segments, &diar_segments);
        utterances = align::merge_adjacent(utterances, self.opts.merge_gap_ms);

        // ---- 标点恢复 ----
        progress(Progress {
            stage: Stage::Punctuation,
            fraction: 0.0,
            detail: "标点恢复".into(),
        });
        let total = utterances.len().max(1);
        for (i, u) in utterances.iter_mut().enumerate() {
            if i % 16 == 0 {
                cancel.check()?;
            }
            u.text = self.punctuate(&u.text);
            progress(Progress {
                stage: Stage::Punctuation,
                fraction: (i + 1) as f32 / total as f32,
                detail: format!("{}/{}", i + 1, total),
            });
        }

        // 重新编号，保证 utterance_id 连续——纪要里的 [#id] 引用依赖它（§4.4）。
        for (i, u) in utterances.iter_mut().enumerate() {
            u.id = i as i64 + 1;
            if u.source == Source::Mic {
                u.speaker_id = LOCAL_SPEAKER_ID.to_string();
            }
        }

        progress(Progress {
            stage: Stage::Done,
            fraction: 1.0,
            detail: format!("{} 条发言", utterances.len()),
        });

        Ok(utterances)
    }
}

fn samples_to_ms(samples: usize) -> u32 {
    ((samples as u64 * 1000) / TARGET_SAMPLE_RATE as u64) as u32
}

fn require_target_rate(pcm: &Pcm) -> Result<()> {
    if pcm.sample_rate != TARGET_SAMPLE_RATE {
        return Err(Error::audio(format!(
            "expected {}Hz, got {}Hz — 请先调用 audio::resample_to_target",
            TARGET_SAMPLE_RATE, pcm.sample_rate
        )));
    }
    Ok(())
}

fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Config(format!("路径不是合法 UTF-8: {}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_flips() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
        t.cancel();
        assert!(t.is_cancelled());
        assert!(t.check().unwrap_err().is_cancelled());
    }

    #[test]
    fn samples_convert_to_milliseconds() {
        assert_eq!(samples_to_ms(0), 0);
        assert_eq!(samples_to_ms(16_000), 1000);
        assert_eq!(samples_to_ms(8_000), 500);
    }

    #[test]
    fn rejects_wrong_sample_rate() {
        let pcm = Pcm {
            sample_rate: 44_100,
            samples: vec![0.0; 10],
        };
        assert!(require_target_rate(&pcm).is_err());
    }
}
