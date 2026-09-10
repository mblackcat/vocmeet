//! 出网守卫。对应 §5「隐私边界」。
//!
//! 这是**所有对外 HTTP 出口的唯一收口**。MVP 只做两档策略与本地审计行，
//! 不做域名白名单、不做 PII 脱敏正则——但收口本身现在就必须存在：
//! 事后再补要翻遍整个代码库，现在做只是几十行。
//!
//! 三件事在请求发出前完成：
//! 1. 校验端点是否被当前策略允许；
//! 2. 对说话人真名做替换（A/B/C），生成后由调用方回填；
//! 3. 写审计行（端点、模型、字符数，**不记内容**）。

use std::collections::HashMap;

use crate::{Error, Result, Utterance};

/// 出网策略。MVP 两档，v1.0 会扩展出 `Allowlist`（§9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressPolicy {
    /// 出厂默认：只允许本机端点（Ollama / 本地 vLLM）。
    LocalOnly,
    /// 允许任意端点。UI 上必须明确告知逐字稿将离开本机。
    Open,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        EgressPolicy::LocalOnly
    }
}

impl EgressPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            EgressPolicy::LocalOnly => "local_only",
            EgressPolicy::Open => "open",
        }
    }
}

/// 一条审计记录。**不含请求内容**，只有元数据。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressRecord {
    pub endpoint: String,
    pub model: String,
    pub purpose: String,
    pub chars_sent: usize,
    pub redacted: bool,
    pub policy: String,
}

/// 校验端点是否被策略允许。
///
/// `LocalOnly` 下只放行 localhost / 127.0.0.1 / ::1。注意这里是**拒绝优先**：
/// 解析不出主机名一律拒绝，而不是放行。
pub fn check_endpoint(policy: EgressPolicy, endpoint: &str) -> Result<()> {
    match policy {
        EgressPolicy::Open => Ok(()),
        EgressPolicy::LocalOnly => {
            let host = extract_host(endpoint).ok_or_else(|| Error::EgressBlocked {
                policy: policy.as_str().to_string(),
                reason: format!("无法从 `{endpoint}` 解析主机名"),
            })?;
            if is_loopback(&host) {
                Ok(())
            } else {
                Err(Error::EgressBlocked {
                    policy: policy.as_str().to_string(),
                    reason: format!(
                        "端点 `{host}` 不是本机地址。当前策略仅允许本地端点；\
                         如需使用云端模型，请在设置中切换到「允许任意端点」并确认逐字稿将离开本机。"
                    ),
                })
            }
        }
    }
}

fn extract_host(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(endpoint);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map(|(_, a)| a).unwrap_or(authority);

    // IPv6 字面量 [::1]:11434
    if let Some(end) = authority.strip_prefix('[') {
        let (h, _) = end.split_once(']')?;
        return Some(h.to_ascii_lowercase());
    }
    let host = authority.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

pub fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1")
        || host.starts_with("127.")
}

/// 判断一个端点 URL 是否指向本机地址。
pub fn is_local_endpoint(endpoint: &str) -> bool {
    extract_host(endpoint)
        .map(|h| is_loopback(&h))
        .unwrap_or(false)
}

/// 说话人真名脱敏。
///
/// §5 规定：真名默认不出本机。这里把每个不同的说话人替换成稳定的 A/B/C…，
/// 并返回映射表供生成后回填。
pub struct Redaction {
    /// 化名 -> 真名，用于把 LLM 输出里的 A/B/C 换回去。
    pub alias_to_real: HashMap<String, String>,
}

impl Redaction {
    /// 对发言列表做脱敏，返回脱敏后的副本与映射。
    pub fn apply(utterances: &[Utterance]) -> (Vec<Utterance>, Redaction) {
        let mut alias_to_real = HashMap::new();
        let mut real_to_alias: HashMap<String, String> = HashMap::new();
        let mut next = 0usize;

        let redacted = utterances
            .iter()
            .map(|u| {
                let real = u.display_speaker().to_string();
                let alias = real_to_alias.entry(real.clone()).or_insert_with(|| {
                    let a = alias_for(next);
                    next += 1;
                    alias_to_real.insert(a.clone(), real.clone());
                    a
                });
                let mut c = u.clone();
                c.speaker_name = Some(alias.clone());
                c
            })
            .collect();

        (redacted, Redaction { alias_to_real })
    }

    /// 把生成的纪要里的化名换回真名。
    pub fn restore(&self, text: &str) -> String {
        // 长化名优先替换，避免 "A" 先把 "AA" 的前缀吃掉。
        let mut pairs: Vec<(&String, &String)> = self.alias_to_real.iter().collect();
        pairs.sort_by_key(|(alias, _)| std::cmp::Reverse(alias.len()));

        let mut out = text.to_string();
        for (alias, real) in pairs {
            out = out.replace(alias.as_str(), real.as_str());
        }
        out
    }
}

/// 0 -> "发言人A", 1 -> "发言人B", ..., 26 -> "发言人AA"
fn alias_for(index: usize) -> String {
    let mut n = index;
    let mut letters = String::new();
    loop {
        letters.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    format!("发言人{letters}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn u(id: i64, name: &str) -> Utterance {
        Utterance {
            id,
            speaker_id: format!("spk_{id}"),
            speaker_name: Some(name.into()),
            start_ms: 0,
            end_ms: 100,
            text: format!("{name}的发言"),
            source: Source::System,
            low_confidence: false,
        }
    }

    #[test]
    fn local_only_allows_loopback_endpoints() {
        for ep in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8000/v1",
            "http://[::1]:11434/v1",
            "https://127.0.0.5/v1",
        ] {
            check_endpoint(EgressPolicy::LocalOnly, ep)
                .unwrap_or_else(|e| panic!("{ep} 应被放行: {e}"));
        }
    }

    #[test]
    fn local_only_blocks_remote_endpoints() {
        for ep in [
            "https://api.openai.com/v1",
            "http://192.168.1.10:11434/v1",
            "https://generativelanguage.googleapis.com/v1beta",
        ] {
            let err = check_endpoint(EgressPolicy::LocalOnly, ep).unwrap_err();
            assert!(matches!(err, Error::EgressBlocked { .. }), "{ep} 应被拦截");
        }
    }

    #[test]
    fn local_only_blocks_unparseable_endpoint() {
        assert!(check_endpoint(EgressPolicy::LocalOnly, "").is_err());
        assert!(check_endpoint(EgressPolicy::LocalOnly, "://").is_err());
    }

    #[test]
    fn open_policy_allows_everything() {
        check_endpoint(EgressPolicy::Open, "https://api.openai.com/v1").unwrap();
    }

    #[test]
    fn userinfo_does_not_fool_host_extraction() {
        // 经典绕过：user@evil.com 写成 localhost@evil.com
        let err = check_endpoint(EgressPolicy::LocalOnly, "http://localhost@evil.com/v1");
        assert!(err.is_err(), "应取 @ 之后的真实主机名");
    }

    #[test]
    fn redaction_replaces_names_and_restores_them() {
        let src = vec![u(1, "张三"), u(2, "李四"), u(3, "张三")];
        let (redacted, map) = Redaction::apply(&src);

        assert_eq!(redacted[0].speaker_name.as_deref(), Some("发言人A"));
        assert_eq!(redacted[1].speaker_name.as_deref(), Some("发言人B"));
        // 同一个人必须拿到同一个化名
        assert_eq!(redacted[2].speaker_name.as_deref(), Some("发言人A"));

        let summary = "发言人A 提出方案，发言人B 表示同意。";
        assert_eq!(map.restore(summary), "张三 提出方案，李四 表示同意。");
    }

    #[test]
    fn alias_sequence_extends_past_z() {
        assert_eq!(alias_for(0), "发言人A");
        assert_eq!(alias_for(25), "发言人Z");
        assert_eq!(alias_for(26), "发言人AA");
    }
}
