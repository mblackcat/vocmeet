//! VocMeet core: 离线会议纪要系统的核心库。
//!
//! 模块划分与 `docs/vocmeet_mvp_plan.md` 的 §4 一一对应：
//!
//! - [`audio`]     WAV 读写、重采样、分片落盘（§4.1）
//! - [`asr`]       转写引擎抽象与 sherpa-onnx 实现（§4.2）
//! - [`align`]     ASR 段与 diarization 段的时间轴对齐（§4.2）
//! - [`store`]     SQLCipher 存储与 OS Keychain 密钥托管（§4.6、§5）
//! - [`models`]    ONNX 权重清单、下载与 SHA256 校验（§6）
//! - [`policy`]    出网守卫：所有对外 HTTP 出口的唯一收口（§5）
//! - [`llm`]       OpenAI 兼容客户端与 SSE 解析（§4.5）
//! - [`summarize`] 模板渲染与 map-reduce 纪要编排（§4.4）
//! - [`jobs`]      任务状态机与进度事件（§6）

pub mod align;
pub mod asr;
pub mod audio;
pub mod config;
pub mod error;
pub mod jobs;
pub mod llm;
pub mod models;
pub mod policy;
pub mod store;
pub mod summarize;
pub mod update;

pub use error::{Error, Result};

/// 一条发言。对应 `docs/vocmeet_mvp_plan.md` §4.2 的接口契约。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Utterance {
    pub id: i64,
    /// "user" 表示本机用户（麦克风轨），"spk_0"/"spk_1"… 表示系统轨聚类结果。
    pub speaker_id: String,
    /// 人工命名后回填。
    pub speaker_name: Option<String>,
    pub start_ms: u32,
    pub end_ms: u32,
    pub text: String,
    pub source: Source,
    /// 对齐置信度不足，UI 需高亮提示校对。见 §4.2 对齐规则。
    pub low_confidence: bool,
}

impl Utterance {
    /// UI 与提示词里展示的说话人标签：优先真名，回落到 speaker_id。
    pub fn display_speaker(&self) -> &str {
        self.speaker_name.as_deref().unwrap_or(&self.speaker_id)
    }
}

/// 发言来自哪一条音轨。双轨录制策略见 §4.1。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// 麦克风轨：说话人恒为本机用户。
    Mic,
    /// 系统回环轨：需要做 diarization。
    System,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Mic => "mic",
            Source::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mic" => Some(Source::Mic),
            "system" => Some(Source::System),
            _ => None,
        }
    }
}

/// 本机用户在 `speaker_id` 中的固定取值。
pub const LOCAL_SPEAKER_ID: &str = "user";

/// 全流程统一使用的采样率。所有采集与推理都在 16kHz 单声道上进行。
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// 毫秒时间戳格式化为 `HH:MM:SS`，用于提示词与导出。
pub fn format_ts(ms: u32) -> String {
    let total_secs = ms / 1000;
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_ts_pads_components() {
        assert_eq!(format_ts(0), "00:00:00");
        assert_eq!(format_ts(1_000), "00:00:01");
        assert_eq!(format_ts(61_000), "00:01:01");
        assert_eq!(format_ts(3_661_000), "01:01:01");
    }

    #[test]
    fn display_speaker_prefers_real_name() {
        let mut u = Utterance {
            id: 1,
            speaker_id: "spk_0".into(),
            speaker_name: None,
            start_ms: 0,
            end_ms: 100,
            text: "hi".into(),
            source: Source::System,
            low_confidence: false,
        };
        assert_eq!(u.display_speaker(), "spk_0");
        u.speaker_name = Some("张三".into());
        assert_eq!(u.display_speaker(), "张三");
    }

    #[test]
    fn source_roundtrips() {
        for s in [Source::Mic, Source::System] {
            assert_eq!(Source::parse(s.as_str()), Some(s));
        }
        assert_eq!(Source::parse("nope"), None);
    }
}
