//! ASR 段与 diarization 段的时间轴对齐。对应 §4.2 的「对齐规则」与「说话人合并」。
//!
//! 这里是整个转写链路里唯一会产生「系统性误差」的地方，因此策略写得保守：
//! 归属置信度不足时不猜，而是打 `low_confidence` 标记交 UI 高亮、由人校对。

use crate::{Source, Utterance, LOCAL_SPEAKER_ID};

/// VAD 切出的一段语音（含采样，供 ASR 使用）。
#[derive(Debug, Clone)]
pub struct SpeechSegment {
    pub start_ms: u32,
    pub end_ms: u32,
    pub samples: Vec<f32>,
}

/// ASR 输出的一段文本。
#[derive(Debug, Clone)]
pub struct AsrSegment {
    pub start_ms: u32,
    pub end_ms: u32,
    pub text: String,
    /// 词级时间戳（整轨坐标，毫秒）。sherpa-onnx 返回 `Option`，随模型而定。
    pub word_times_ms: Option<Vec<u32>>,
    pub source: Source,
}

/// diarization 输出的一段说话人归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiarSegment {
    pub start_ms: u32,
    pub end_ms: u32,
    pub speaker: i32,
}

/// 重叠比例低于此值即判为归属不可靠，打 low_confidence。见 §4.2。
const MIN_OVERLAP_RATIO: f32 = 0.5;

/// 把 ASR 段按时间重叠比例归属到 diarization 段。
///
/// - 麦克风轨不参与 diarization，说话人恒为本机用户（§4.1 双轨策略的收益）。
/// - 系统轨按「与各 diarization 段的重叠时长」取最大者归属。
/// - 最大重叠占本段时长的比例 < [`MIN_OVERLAP_RATIO`]，或该段横跨多个说话人时，
///   标记 `low_confidence`。
pub fn assign_speakers(asr: Vec<AsrSegment>, diar: &[DiarSegment]) -> Vec<Utterance> {
    let mut out: Vec<Utterance> = asr
        .into_iter()
        .map(|seg| {
            if seg.source == Source::Mic {
                return Utterance {
                    id: 0,
                    speaker_id: LOCAL_SPEAKER_ID.to_string(),
                    speaker_name: None,
                    start_ms: seg.start_ms,
                    end_ms: seg.end_ms,
                    text: seg.text,
                    source: seg.source,
                    low_confidence: false,
                };
            }

            let (speaker, ratio, spanned) = best_speaker(&seg, diar);
            let speaker_id = match speaker {
                Some(s) => format!("spk_{s}"),
                // 完全没有重叠：diarization 没覆盖到这段（常见于极短插话）。
                None => "spk_unknown".to_string(),
            };

            Utterance {
                id: 0,
                speaker_id,
                speaker_name: None,
                start_ms: seg.start_ms,
                end_ms: seg.end_ms,
                text: seg.text,
                source: seg.source,
                low_confidence: speaker.is_none() || ratio < MIN_OVERLAP_RATIO || spanned > 1,
            }
        })
        .collect();

    out.sort_by_key(|u| (u.start_ms, u.end_ms));
    out
}

/// 返回 (最佳说话人, 重叠占比, 横跨的说话人数)。
fn best_speaker(seg: &AsrSegment, diar: &[DiarSegment]) -> (Option<i32>, f32, usize) {
    let seg_len = seg.end_ms.saturating_sub(seg.start_ms).max(1) as f32;

    let mut best: Option<(i32, u32)> = None;
    let mut spanned = 0usize;

    for d in diar {
        let ov = overlap_ms(seg.start_ms, seg.end_ms, d.start_ms, d.end_ms);
        if ov == 0 {
            continue;
        }
        spanned += 1;
        match best {
            Some((_, best_ov)) if best_ov >= ov => {}
            _ => best = Some((d.speaker, ov)),
        }
    }

    match best {
        Some((spk, ov)) => (Some(spk), ov as f32 / seg_len, spanned),
        None => (None, 0.0, 0),
    }
}

fn overlap_ms(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> u32 {
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    end.saturating_sub(start)
}

/// 合并相邻同说话人的发言。
///
/// 只有间隔小于 `max_gap_ms` 才合并——v1 草案缺这个判断，会把跨越数分钟静音的
/// 两段粘成一段。中文拼接不插空格（§4.2）。
pub fn merge_adjacent(utterances: Vec<Utterance>, max_gap_ms: u32) -> Vec<Utterance> {
    let mut out: Vec<Utterance> = Vec::with_capacity(utterances.len());

    for u in utterances {
        let should_merge = match out.last() {
            Some(prev) => {
                prev.speaker_id == u.speaker_id
                    && prev.source == u.source
                    && u.start_ms.saturating_sub(prev.end_ms) < max_gap_ms
            }
            None => false,
        };

        if should_merge {
            let prev = out.last_mut().expect("checked above");
            prev.text = join_text(&prev.text, &u.text);
            prev.end_ms = u.end_ms.max(prev.end_ms);
            // 任一片段不可靠，合并结果就不可靠。
            prev.low_confidence |= u.low_confidence;
        } else {
            out.push(u);
        }
    }

    out
}

/// 拼接两段文本：两侧都是 CJK 时不插空格，否则插一个空格。
fn join_text(left: &str, right: &str) -> String {
    let l = left.trim_end();
    let r = right.trim_start();
    if l.is_empty() {
        return r.to_string();
    }
    if r.is_empty() {
        return l.to_string();
    }
    let need_space = !(is_cjk(l.chars().next_back()) && is_cjk(r.chars().next()));
    if need_space {
        format!("{l} {r}")
    } else {
        format!("{l}{r}")
    }
}

fn is_cjk(c: Option<char>) -> bool {
    matches!(c, Some(c) if matches!(c as u32,
        0x3000..=0x303F   // CJK 标点
        | 0x4E00..=0x9FFF // 基本汉字
        | 0x3400..=0x4DBF // 扩展 A
        | 0xFF00..=0xFFEF // 全角字符
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asr(start: u32, end: u32, text: &str, source: Source) -> AsrSegment {
        AsrSegment {
            start_ms: start,
            end_ms: end,
            text: text.into(),
            word_times_ms: None,
            source,
        }
    }

    fn diar(start: u32, end: u32, speaker: i32) -> DiarSegment {
        DiarSegment {
            start_ms: start,
            end_ms: end,
            speaker,
        }
    }

    #[test]
    fn overlap_is_symmetric_and_clamped() {
        assert_eq!(overlap_ms(0, 100, 50, 150), 50);
        assert_eq!(overlap_ms(50, 150, 0, 100), 50);
        assert_eq!(overlap_ms(0, 100, 200, 300), 0);
        assert_eq!(overlap_ms(0, 100, 0, 100), 100);
    }

    #[test]
    fn mic_track_always_maps_to_local_user() {
        let out = assign_speakers(vec![asr(0, 1000, "我说的", Source::Mic)], &[]);
        assert_eq!(out[0].speaker_id, LOCAL_SPEAKER_ID);
        assert!(!out[0].low_confidence, "麦克风轨不该被标为低置信");
    }

    #[test]
    fn system_track_takes_max_overlap_speaker() {
        let d = vec![diar(0, 600, 0), diar(600, 1000, 1)];
        let out = assign_speakers(vec![asr(0, 1000, "你好", Source::System)], &d);
        // 与 spk_0 重叠 600ms > spk_1 的 400ms
        assert_eq!(out[0].speaker_id, "spk_0");
        // 横跨两个说话人 -> 低置信
        assert!(out[0].low_confidence);
    }

    #[test]
    fn clean_single_speaker_segment_is_confident() {
        let d = vec![diar(0, 2000, 3)];
        let out = assign_speakers(vec![asr(100, 900, "确定", Source::System)], &d);
        assert_eq!(out[0].speaker_id, "spk_3");
        assert!(!out[0].low_confidence);
    }

    #[test]
    fn no_overlap_marks_unknown_and_low_confidence() {
        let d = vec![diar(5000, 6000, 0)];
        let out = assign_speakers(vec![asr(0, 1000, "孤立", Source::System)], &d);
        assert_eq!(out[0].speaker_id, "spk_unknown");
        assert!(out[0].low_confidence);
    }

    #[test]
    fn low_overlap_ratio_marks_low_confidence() {
        // 段长 1000ms，但只有 300ms 落在 diarization 段内 -> 30% < 50%
        let d = vec![diar(700, 1000, 2)];
        let out = assign_speakers(vec![asr(0, 1000, "半个", Source::System)], &d);
        assert_eq!(out[0].speaker_id, "spk_2");
        assert!(out[0].low_confidence);
    }

    #[test]
    fn merge_joins_same_speaker_within_gap() {
        let d = vec![diar(0, 10_000, 0)];
        let out = assign_speakers(
            vec![
                asr(0, 1000, "今天的议题", Source::System),
                asr(1500, 2500, "是预算", Source::System),
            ],
            &d,
        );
        let merged = merge_adjacent(out, 2000);
        assert_eq!(merged.len(), 1);
        // 中文之间不插空格
        assert_eq!(merged[0].text, "今天的议题是预算");
        assert_eq!(merged[0].end_ms, 2500);
    }

    #[test]
    fn merge_respects_gap_threshold() {
        let d = vec![diar(0, 60_000, 0)];
        let out = assign_speakers(
            vec![
                asr(0, 1000, "前半段", Source::System),
                // 间隔 30 秒，不该合并
                asr(31_000, 32_000, "后半段", Source::System),
            ],
            &d,
        );
        let merged = merge_adjacent(out, 2000);
        assert_eq!(merged.len(), 2, "跨越长静音的两段不应合并");
    }

    #[test]
    fn merge_does_not_join_different_speakers() {
        let d = vec![diar(0, 1000, 0), diar(1000, 2000, 1)];
        let out = assign_speakers(
            vec![
                asr(0, 1000, "甲说", Source::System),
                asr(1000, 2000, "乙说", Source::System),
            ],
            &d,
        );
        let merged = merge_adjacent(out, 2000);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_propagates_low_confidence() {
        let d = vec![diar(0, 10_000, 0)];
        let mut out = assign_speakers(
            vec![
                asr(0, 1000, "稳", Source::System),
                asr(1200, 2000, "不稳", Source::System),
            ],
            &d,
        );
        out[1].low_confidence = true;
        let merged = merge_adjacent(out, 2000);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].low_confidence, "任一片段不可靠则合并结果不可靠");
    }

    #[test]
    fn join_text_inserts_space_only_for_latin() {
        assert_eq!(join_text("中文", "拼接"), "中文拼接");
        assert_eq!(join_text("hello", "world"), "hello world");
        assert_eq!(join_text("中文", "english"), "中文 english");
        assert_eq!(join_text("", "开头"), "开头");
    }

    #[test]
    fn output_is_sorted_by_start_time() {
        let d = vec![diar(0, 10_000, 0)];
        let out = assign_speakers(
            vec![
                asr(5000, 6000, "后", Source::System),
                asr(1000, 2000, "先", Source::System),
            ],
            &d,
        );
        assert_eq!(out[0].text, "先");
        assert_eq!(out[1].text, "后");
    }
}
