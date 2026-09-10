//! 任务状态机。对应 §6「可靠性」。
//!
//! MVP 单并发串行——多任务争抢 CPU 会让整机卡顿，而验证阶段没有并发需求。

use crate::asr::Stage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Transcribe,
    Summarize,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Transcribe => "transcribe",
            JobKind::Summarize => "summarize",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Cancelled,
    Failed(String),
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Cancelled => "cancelled",
            JobState::Failed(_) => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Cancelled | JobState::Failed(_)
        )
    }
}

/// 推给 UI 的进度事件。
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobEvent {
    pub meeting_id: i64,
    pub kind: &'static str,
    pub state: String,
    /// 0.0 ~ 1.0，整体进度（跨阶段加权）。
    pub progress: f32,
    pub stage: Option<Stage>,
    pub detail: String,
}

/// 把各阶段进度加权成整体百分比。
///
/// 权重按实测耗时占比粗调：ASR 最重，diarization 次之。W1 拿到真实 RTF 后再校准。
pub fn overall_progress(stage: Stage, fraction: f32) -> f32 {
    let f = fraction.clamp(0.0, 1.0);
    let (base, span) = match stage {
        Stage::LoadingModels => (0.00, 0.05),
        Stage::Vad => (0.05, 0.10),
        Stage::Asr => (0.15, 0.55),
        Stage::Diarization => (0.70, 0.20),
        Stage::Punctuation => (0.90, 0.09),
        Stage::Done => (1.00, 0.00),
    };
    (base + span * f).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_monotonic_across_stages() {
        let seq = [
            overall_progress(Stage::LoadingModels, 1.0),
            overall_progress(Stage::Vad, 1.0),
            overall_progress(Stage::Asr, 0.5),
            overall_progress(Stage::Asr, 1.0),
            overall_progress(Stage::Diarization, 1.0),
            overall_progress(Stage::Punctuation, 1.0),
            overall_progress(Stage::Done, 1.0),
        ];
        for w in seq.windows(2) {
            assert!(w[1] >= w[0], "进度不能倒退: {:?}", seq);
        }
        assert_eq!(seq[seq.len() - 1], 1.0);
    }

    #[test]
    fn progress_is_clamped() {
        assert_eq!(overall_progress(Stage::Asr, -5.0), 0.15);
        assert!(overall_progress(Stage::Asr, 99.0) <= 1.0);
    }

    #[test]
    fn terminal_states_are_terminal() {
        assert!(!JobState::Queued.is_terminal());
        assert!(!JobState::Running.is_terminal());
        assert!(JobState::Done.is_terminal());
        assert!(JobState::Cancelled.is_terminal());
        assert!(JobState::Failed("boom".into()).is_terminal());
    }

    #[test]
    fn job_state_serializes_with_detail() {
        let json = serde_json::to_string(&JobState::Failed("磁盘写满".into())).unwrap();
        assert!(json.contains("failed"));
        assert!(json.contains("磁盘写满"));
    }
}
