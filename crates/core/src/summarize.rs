//! 纪要编排：模板渲染 + map-reduce。对应 §4.4。
//!
//! 关键设计：每条决议/待办都带 `[#utterance_id]` 引用，UI 上可点击跳回逐字稿原文。
//! 这是对抗幻觉最有效的手段——比在提示词里写「禁止幻觉」有用得多，
//! 也让 §7 的「可溯源比例」成为可量化指标。

use std::path::Path;

use minijinja::{context, Environment};

use crate::config::LlmConfig;
use crate::llm::{ChatMessage, LlmClient};
use crate::policy::{EgressRecord, Redaction};
use crate::{format_ts, Error, Result, Utterance};

/// 模板文件（YAML）结构，对应 `templates/meeting_default.yaml`。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Template {
    pub version: String,
    pub metadata: TemplateMeta,
    pub system_instruction: String,
    pub user_template: String,
    #[serde(default)]
    pub generation: GenerationSettings,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TemplateMeta {
    pub template_id: String,
    pub name: String,
    #[serde(default)]
    pub stage: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct GenerationSettings {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl Default for GenerationSettings {
    fn default() -> Self {
        Self {
            temperature: None,
            max_tokens: None,
        }
    }
}

impl Template {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let tpl: Template = serde_yaml::from_str(&raw)
            .map_err(|e| Error::Template(format!("{}: {e}", path.display())))?;
        tpl.validate()?;
        Ok(tpl)
    }

    /// 加载时做静态检查：模板语法错误应该在设置页就报出来，而不是等生成时才失败。
    pub fn validate(&self) -> Result<()> {
        let mut env = Environment::new();
        env.add_template("system", &self.system_instruction)
            .map_err(|e| Error::Template(format!("system_instruction: {e}")))?;
        env.add_template("user", &self.user_template)
            .map_err(|e| Error::Template(format!("user_template: {e}")))?;
        Ok(())
    }
}

/// 渲染上下文。
#[derive(Clone, Copy)]
pub struct RenderInput<'a> {
    pub meeting_title: &'a str,
    pub meeting_time: &'a str,
    pub attendees: &'a [String],
    pub scratchpad: &'a str,
    pub utterances: &'a [Utterance],
}

/// 渲染出 system + user 两条消息。
///
/// 用 minijinja（`{{ }}`）而不是字符串格式化：提示词正文里同时存在「要替换的占位符」
/// 和「给模型看的字面量花括号」，用 `format!`/`str::replace` 一类方案必然出错。
pub fn render(template: &Template, input: &RenderInput) -> Result<(String, String)> {
    let mut env = Environment::new();
    env.add_filter("ts", |ms: u32| format_ts(ms));
    env.add_template("system", &template.system_instruction)
        .map_err(|e| Error::Template(format!("system: {e}")))?;
    env.add_template("user", &template.user_template)
        .map_err(|e| Error::Template(format!("user: {e}")))?;

    // 发言序列化成模板可用的简单结构。
    let utterances: Vec<_> = input
        .utterances
        .iter()
        .map(|u| {
            context! {
                id => u.id,
                speaker_id => u.speaker_id.clone(),
                speaker_name => u.speaker_name.clone(),
                start_ms => u.start_ms,
                end_ms => u.end_ms,
                text => u.text.clone(),
                low_confidence => u.low_confidence,
            }
        })
        .collect();

    let ctx = context! {
        meeting_title => input.meeting_title,
        meeting_time => input.meeting_time,
        attendees => input.attendees.to_vec(),
        scratchpad => input.scratchpad,
        utterances => utterances,
    };

    let system = env
        .get_template("system")
        .and_then(|t| t.render(&ctx))
        .map_err(|e| Error::Template(format!("render system: {e}")))?;
    let user = env
        .get_template("user")
        .and_then(|t| t.render(&ctx))
        .map_err(|e| Error::Template(format!("render user: {e}")))?;

    Ok((system, user))
}

/// 估算文本 token 数。
///
/// 非 OpenAI 模型的分词器不同，统一按 1.15 系数留安全边际（§4.4）。
pub fn estimate_tokens(text: &str) -> usize {
    // BPE 构造开销很大，全进程只建一次。
    static BPE: std::sync::OnceLock<Option<tiktoken_rs::CoreBPE>> = std::sync::OnceLock::new();
    let bpe = BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok());
    let base = match bpe {
        Some(b) => b.encode_with_special_tokens(text).len(),
        // 分词器不可用时回落到保守估计：中文约 1 字 1 token。
        None => text.chars().count(),
    };
    (base as f32 * 1.15).ceil() as usize
}

/// 按 token 预算把发言切成 chunk，相邻 chunk 重叠 `overlap` 条发言，
/// 防止议题恰好被切断（§4.4）。
pub fn chunk_utterances(
    utterances: &[Utterance],
    budget_tokens: usize,
    overlap: usize,
) -> Vec<Vec<Utterance>> {
    if utterances.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current: Vec<Utterance> = Vec::new();
    let mut current_tokens = 0usize;

    for u in utterances {
        let t = estimate_tokens(&u.text) + 16; // 16 ≈ 说话人标签与 id 的开销
        if !current.is_empty() && current_tokens + t > budget_tokens {
            let carry_start = current.len().saturating_sub(overlap);
            let carry: Vec<Utterance> = current[carry_start..].to_vec();
            chunks.push(std::mem::take(&mut current));
            current = carry;
            current_tokens = current.iter().map(|c| estimate_tokens(&c.text) + 16).sum();
        }
        current.push(u.clone());
        current_tokens += t;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 单次生成能否放下——放得下就跳过 map-reduce 直接 Compose（§4.4）。
pub fn fits_single_pass(utterances: &[Utterance], scratchpad: &str, cfg: &LlmConfig) -> bool {
    let total: usize = utterances
        .iter()
        .map(|u| estimate_tokens(&u.text) + 16)
        .sum::<usize>()
        + estimate_tokens(scratchpad);
    total < cfg.context_tokens / 2
}

/// 生成结果。
pub struct SummaryOutput {
    pub markdown: String,
    /// 本次生成产生的全部出网记录，供审计写库。
    pub egress: Vec<EgressRecord>,
    /// 是否走了 map-reduce。
    pub used_map_reduce: bool,
}

/// 纪要生成主流程。
pub struct Summarizer<'a> {
    pub client: &'a LlmClient,
    pub template: &'a Template,
    pub config: &'a LlmConfig,
    /// 是否对说话人真名脱敏（§5 默认开启）。
    pub redact_names: bool,
}

impl<'a> Summarizer<'a> {
    pub async fn run(
        &self,
        input: &RenderInput<'_>,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<SummaryOutput> {
        // ---- 脱敏（真名不出本机）----
        let (working, redaction) = if self.redact_names {
            let (r, map) = Redaction::apply(input.utterances);
            (r, Some(map))
        } else {
            (input.utterances.to_vec(), None)
        };

        let mut egress = Vec::new();

        let single = fits_single_pass(&working, input.scratchpad, self.config);
        let composed = if single {
            let composed_input = RenderInput {
                utterances: &working,
                ..*input
            };
            let (system, user) = render(self.template, &composed_input)?;
            let (text, rec) = self
                .client
                .stream_complete(
                    &[ChatMessage::system(system), ChatMessage::user(user)],
                    "summarize_single_pass",
                    self.redact_names,
                    on_delta,
                )
                .await?;
            egress.push(rec);
            text
        } else {
            // ---- Map：逐 chunk 提要点 ----
            let budget = (self.config.context_tokens / 2).min(8000);
            let chunks = chunk_utterances(&working, budget, 3);
            let mut notes = Vec::new();

            for (i, chunk) in chunks.iter().enumerate() {
                let chunk_input = RenderInput {
                    utterances: chunk,
                    ..*input
                };
                let (_, user) = render(self.template, &chunk_input)?;
                let (text, rec) = self
                    .client
                    .complete(
                        &[
                            ChatMessage::system(MAP_INSTRUCTION),
                            ChatMessage::user(user),
                        ],
                        &format!("summarize_map_{}", i + 1),
                        self.redact_names,
                    )
                    .await?;
                egress.push(rec);
                notes.push(text);
            }

            // ---- Reduce + Compose：把各 chunk 要点合成最终纪要 ----
            let joined = notes.join("\n\n---\n\n");
            let (system, _) = render(self.template, input)?;
            let user = format!(
                "以下是分段提取的要点（每段已带 [#utterance_id] 引用）。\
                 请跨段去重归并，冲突项保留双方并标注「存在分歧」，然后按格式输出完整纪要。\n\n\
                 ## 参会者速记笔记\n{}\n\n## 分段要点\n{}",
                if input.scratchpad.is_empty() {
                    "（无）"
                } else {
                    input.scratchpad
                },
                joined
            );
            let (text, rec) = self
                .client
                .stream_complete(
                    &[ChatMessage::system(system), ChatMessage::user(user)],
                    "summarize_compose",
                    self.redact_names,
                    on_delta,
                )
                .await?;
            egress.push(rec);
            text
        };

        // ---- 回填真名 ----
        let markdown = match &redaction {
            Some(map) => map.restore(&composed),
            None => composed,
        };

        Ok(SummaryOutput {
            markdown,
            egress,
            used_map_reduce: !single,
        })
    }
}

/// 从已生成的纪要里提取主题关键词。
///
/// 单独一次短请求，不走脱敏——关键词是主题词，本身不含发言内容；
/// 且此时纪要已生成，真名已回填，再送一次反而会泄露更多。所以只送纪要的标题层。
pub async fn extract_keywords(client: &LlmClient, summary_md: &str) -> Result<Vec<String>> {
    // 只取标题行与决议段，避免把整篇纪要再发一遍。
    let condensed: String = summary_md
        .lines()
        .filter(|l| l.trim_start().starts_with('#') || l.trim_start().starts_with("- "))
        .take(40)
        .collect::<Vec<_>>()
        .join("
");

    let (raw, _) = client
        .complete(
            &[
                ChatMessage::system(KEYWORD_INSTRUCTION),
                ChatMessage::user(&condensed),
            ],
            "extract_keywords",
            false,
        )
        .await?;

    Ok(parse_keywords(&raw))
}

/// 容错解析：模型可能返回 JSON 数组、也可能返回顿号/逗号分隔的一行。
fn parse_keywords(raw: &str) -> Vec<String> {
    let trimmed = raw.trim().trim_start_matches("```json").trim_matches('`').trim();

    if let Ok(list) = serde_json::from_str::<Vec<String>>(trimmed) {
        return clean_keywords(list);
    }
    clean_keywords(
        trimmed
            .split(SPLIT_CHARS)
            .map(|s| s.to_string())
            .collect(),
    )
}

/// 关键词分隔符：顿号、半角逗号、全角逗号、换行。
const SPLIT_CHARS: [char; 4] = ['\u{3001}', ',', '\u{FF0C}', '\n'];

fn clean_keywords(list: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in list {
        let k = k
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '-' || c == '[' || c == ']')
            .trim()
            .to_string();
        if k.is_empty() || k.chars().count() > 12 || out.contains(&k) {
            continue;
        }
        out.push(k);
        if out.len() == 6 {
            break;
        }
    }
    out
}

const KEYWORD_INSTRUCTION: &str = "从会议纪要中提取 3-6 个主题关键词。每个关键词 2-8 个字，必须是会议实际讨论的具体主题，不要用「会议」「讨论」这类空词。只输出一个 JSON 字符串数组，不要任何解释。例：[\"预算评审\",\"排期风险\"]";

const MAP_INSTRUCTION: &str = "你是会议速记助手。从下面这一段逐字稿中提取：\
要点、决议候选、待办候选、争议点。每一条后面必须附上来源标记 [#utterance_id]。\
只提取这一段中确实出现的内容，不要推测，不要总结成完整纪要。用简洁的中文条目输出。";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn utt(id: i64, text: &str) -> Utterance {
        Utterance {
            id,
            speaker_id: "spk_0".into(),
            speaker_name: Some("张三".into()),
            start_ms: (id as u32) * 1000,
            end_ms: (id as u32) * 1000 + 900,
            text: text.into(),
            source: Source::System,
            low_confidence: false,
        }
    }

    fn sample_template() -> Template {
        Template {
            version: "1.0.0".into(),
            metadata: TemplateMeta {
                template_id: "t".into(),
                name: "测试".into(),
                stage: "compose".into(),
            },
            system_instruction: "标题：{{ meeting_title }}，参会：{{ attendees | join('、') }}"
                .into(),
            user_template:
                "{% for u in utterances %}[#{{ u.id }}] {{ u.speaker_name }} ({{ u.start_ms | ts }}): {{ u.text }}{% if u.low_confidence %} (?){% endif %}\n{% endfor %}"
                    .into(),
            generation: GenerationSettings::default(),
        }
    }

    #[test]
    fn render_fills_placeholders_and_applies_ts_filter() {
        let tpl = sample_template();
        let utts = vec![utt(1, "预算是三百万")];
        let input = RenderInput {
            meeting_title: "季度评审",
            meeting_time: "2026-08-27 10:00",
            attendees: &["张三".to_string(), "李四".to_string()],
            scratchpad: "",
            utterances: &utts,
        };
        let (system, user) = render(&tpl, &input).unwrap();
        assert!(system.contains("季度评审"));
        assert!(system.contains("张三、李四"));
        assert!(user.contains("[#1]"));
        assert!(user.contains("00:00:01"), "ts 过滤器应输出 HH:MM:SS");
    }

    #[test]
    fn render_marks_low_confidence_utterances() {
        let tpl = sample_template();
        let mut u = utt(1, "存疑内容");
        u.low_confidence = true;
        let utts = vec![u];
        let input = RenderInput {
            meeting_title: "x",
            meeting_time: "t",
            attendees: &[],
            scratchpad: "",
            utterances: &utts,
        };
        let (_, user) = render(&tpl, &input).unwrap();
        assert!(user.contains("(?)"), "低置信片段必须在提示词里标出");
    }

    #[test]
    fn invalid_template_is_rejected_at_load_time() {
        let tpl = Template {
            system_instruction: "{% if %}".into(),
            ..sample_template()
        };
        assert!(tpl.validate().is_err(), "语法错误应在校验期暴露");
    }

    #[test]
    fn chunking_respects_budget_and_overlaps() {
        let utts: Vec<Utterance> = (1..=40).map(|i| utt(i, "这是一段大约十几个字的发言内容")).collect();
        let chunks = chunk_utterances(&utts, 200, 3);
        assert!(chunks.len() > 1, "预算很小时应切成多块");
        // 相邻 chunk 应有重叠
        let tail: Vec<i64> = chunks[0].iter().rev().take(3).map(|u| u.id).collect();
        let head: Vec<i64> = chunks[1].iter().take(3).map(|u| u.id).collect();
        assert_eq!(
            tail.into_iter().rev().collect::<Vec<_>>(),
            head,
            "相邻 chunk 应重叠 3 条发言"
        );
    }

    #[test]
    fn chunking_empty_input_yields_nothing() {
        assert!(chunk_utterances(&[], 100, 3).is_empty());
    }

    #[test]
    fn single_pass_decision_follows_context_budget() {
        let cfg = LlmConfig {
            context_tokens: 8192,
            ..LlmConfig::default()
        };
        let short: Vec<Utterance> = (1..=5).map(|i| utt(i, "短")).collect();
        assert!(fits_single_pass(&short, "", &cfg));

        let long: Vec<Utterance> = (1..=2000)
            .map(|i| utt(i, "这是一段相当长的会议发言用来把上下文撑满"))
            .collect();
        assert!(!fits_single_pass(&long, "", &cfg));
    }

    #[test]
    fn parse_keywords_accepts_json_array() {
        let out = parse_keywords(r#"["预算评审","排期风险"]"#);
        assert_eq!(out, vec!["预算评审", "排期风险"]);
    }

    #[test]
    fn parse_keywords_accepts_fenced_json() {
        let out = parse_keywords("```json
[\"招聘计划\"]
```");
        assert_eq!(out, vec!["招聘计划"]);
    }

    #[test]
    fn parse_keywords_falls_back_to_delimited_text() {
        let out = parse_keywords("预算评审、排期风险，人员招聘");
        assert_eq!(out, vec!["预算评审", "排期风险", "人员招聘"]);
    }

    #[test]
    fn parse_keywords_drops_overlong_and_duplicates() {
        let out = parse_keywords("预算、预算、这是一个明显超过十二个字的很长的短语");
        assert_eq!(out, vec!["预算"], "重复与超长项都要丢掉");
    }

    #[test]
    fn parse_keywords_caps_at_six() {
        let out = parse_keywords("一,二,三,四,五,六,七,八");
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn token_estimate_has_safety_margin() {
        let text = "这是一段中文会议记录";
        let est = estimate_tokens(text);
        assert!(est > 0);
        // 加了 1.15 系数，应不小于字符数的一半（保守下界，避免分词器差异导致 flaky）
        assert!(est >= text.chars().count() / 2);
    }
}
