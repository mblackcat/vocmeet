//! OpenAI 兼容客户端与 SSE 解析。对应 §4.5。
//!
//! 这里的每一条健壮性处理都对应一个在真实网关上会崩的坑：
//! - `choices` 可能是空数组（usage-only chunk / 首个 role chunk）→ 跳过，不索引 `[0]`；
//! - SSE 要处理多行 `data:`、`:` 注释行、`\r\n`、跨 chunk 截断的半行；
//! - JSON 解析失败跳过并计数，连续失败超阈值才中止；
//! - 错误响应先读 body 再抛错，否则排障时只有一个裸状态码；
//! - 4xx 不重试，只对 429/5xx/连接错误退避重试。

use std::time::Duration;

use futures_util::StreamExt;

use crate::config::LlmConfig;
use crate::policy::{self, EgressPolicy, EgressRecord};
use crate::{Error, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

pub struct LlmClient {
    http: reqwest::Client,
    config: LlmConfig,
    policy: EgressPolicy,
    api_key: Option<String>,
}

/// 连续多少个 chunk 解析失败后放弃。个别坏行不该让整次生成失败。
const MAX_CONSECUTIVE_PARSE_ERRORS: usize = 20;
const MAX_RETRIES: usize = 3;

impl LlmClient {
    pub fn new(config: LlmConfig, policy: EgressPolicy, api_key: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|e| Error::Llm(format!("build http client: {e}")))?;
        Ok(Self {
            http,
            config,
            policy,
            api_key,
        })
    }

    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// Gemini 用自己的协议：`/v1beta/models/{model}:generateContent`。
    /// 选中 Gemini，或地址落在 generativelanguage，都走这条，不走 OpenAI 兼容层。
    fn is_gemini(&self) -> bool {
        if matches!(
            self.config.provider.as_str(),
            "openai" | "anthropic" | "ollama"
        ) {
            return false;
        }
        self.config.provider == "gemini"
            || self.config.api_format == "gemini"
            || self.config.api_base.contains("generativelanguage.googleapis.com")
    }

    /// 调用地址。Gemini 走原生方法；`responses` 走 `/responses`；其余走 `/chat/completions`。
    /// 用户已经把完整路径写进服务地址时，不再追加。
    fn endpoint(&self, stream: bool) -> String {
        let base = self.config.api_base.trim().trim_end_matches('/');
        if self.is_gemini() {
            let root = gemini_root(base);
            let model = gemini_model_id(&self.config.model);
            let method = if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            let mut url = format!("{root}/models/{model}:{method}");
            if stream {
                url.push_str("?alt=sse");
            }
            return url;
        }
        if base.ends_with("/chat/completions") || base.ends_with("/responses") {
            return base.to_string();
        }
        // chat 与 chat_compat 都走 /chat/completions，只有 responses 换路径。
        if self.config.api_format == "responses" {
            format!("{base}/responses")
        } else {
            format!("{base}/chat/completions")
        }
    }

    fn uses_responses(&self) -> bool {
        !self.is_gemini() && self.endpoint(false).ends_with("/responses")
    }

    /// 出网守卫：策略校验 + 生成审计记录。**所有请求都必须先过这里。**
    fn guard(
        &self,
        messages: &[ChatMessage],
        purpose: &str,
        redacted: bool,
    ) -> Result<EgressRecord> {
        policy::check_endpoint(self.policy, &self.config.api_base)?;
        let chars_sent = messages.iter().map(|m| m.content.chars().count()).sum();
        Ok(EgressRecord {
            endpoint: self.config.api_base.clone(),
            model: self.config.model.clone(),
            purpose: purpose.to_string(),
            chars_sent,
            redacted,
            policy: self.policy.as_str().to_string(),
        })
    }

    fn build_body(&self, messages: &[ChatMessage], stream: bool) -> serde_json::Value {
        if self.is_gemini() {
            let system = messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let contents: Vec<_> = messages
                .iter()
                .filter(|m| m.role != "system")
                .map(|m| {
                    serde_json::json!({
                        "role": if m.role == "assistant" { "model" } else { "user" },
                        "parts": [{ "text": m.content }],
                    })
                })
                .collect();
            let mut body = serde_json::json!({
                "contents": contents,
                "generationConfig": {
                    "temperature": self.config.temperature,
                    "maxOutputTokens": self.config.max_tokens,
                }
            });
            if !system.is_empty() {
                body["systemInstruction"] =
                    serde_json::json!({ "parts": [{ "text": system }] });
            }
            let _ = stream;
            body
        } else if self.uses_responses() {
            let instructions = messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let input: Vec<_> = messages
                .iter()
                .filter(|m| m.role != "system")
                .map(|m| {
                    serde_json::json!({
                        "role": m.role,
                        "content": m.content,
                    })
                })
                .collect();
            let mut body = serde_json::json!({
                "model": self.config.model,
                "input": input,
                "temperature": self.config.temperature,
                "max_output_tokens": self.config.max_tokens,
                "stream": stream,
            });
            if !instructions.is_empty() {
                body["instructions"] = serde_json::Value::String(instructions);
            }
            body
        } else {
            serde_json::json!({
                "model": self.config.model,
                "messages": messages.iter().map(|m| serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })).collect::<Vec<_>>(),
                "temperature": self.config.temperature,
                "max_tokens": self.config.max_tokens,
                "stream": stream,
            })
        }
    }

    fn apply_headers(&self, req: reqwest::RequestBuilder, sse: bool) -> reqwest::RequestBuilder {
        let mut req = req.header("Content-Type", "application/json");
        if sse {
            req = req.header("Accept", "text/event-stream");
        }
        if let Some(key) = &self.api_key {
            if self.is_gemini() {
                req = req.header("x-goog-api-key", key);
            } else {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
        }
        req
    }

    /// 非流式请求。map-reduce 的 Map 阶段用它（中间结果不需要给用户看）。
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        purpose: &str,
        redacted: bool,
    ) -> Result<(String, EgressRecord)> {
        let record = self.guard(messages, purpose, redacted)?;
        let body = self.build_body(messages, false);

        let mut last_err = None;
        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
            }

            match self
                .apply_headers(self.http.post(self.endpoint(false)), false)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        let json: serde_json::Value = r
                            .json()
                            .await
                            .map_err(|e| Error::Llm(format!("解析响应失败: {e}")))?;
                        let text = extract_completion_text(&json).ok_or_else(|| {
                            Error::Llm(format!("响应里没有文本：{json}"))
                        })?;
                        return Ok((text, record));
                    }

                    // 先读 body 再抛错——只有状态码的日志排障成本极高。
                    let body_text = r.text().await.unwrap_or_default();
                    let err = Error::Llm(format!("HTTP {status}: {}", truncate(&body_text, 800)));
                    if !should_retry_status(status.as_u16()) {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
                Err(e) => last_err = Some(Error::Llm(format!("请求失败: {e}"))),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Llm("未知错误".into())))
    }

    /// 流式请求。Compose 阶段用它，让纪要边生成边渲染。
    pub async fn stream_complete(
        &self,
        messages: &[ChatMessage],
        purpose: &str,
        redacted: bool,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<(String, EgressRecord)> {
        let record = self.guard(messages, purpose, redacted)?;
        let body = self.build_body(messages, true);

        let resp = self
            .apply_headers(self.http.post(self.endpoint(true)), true)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Llm(format!("请求失败: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(Error::Llm(format!(
                "HTTP {status}: {}",
                truncate(&body_text, 800)
            )));
        }

        let mut full = String::new();
        let mut parser = SseParser::default();
        let mut consecutive_errors = 0usize;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| Error::Llm(format!("读取流失败: {e}")))?;
            for event in parser.push(&bytes) {
                match event {
                    SseEvent::Done => return Ok((full, record)),
                    SseEvent::Data(payload) => match extract_stream_delta(&payload) {
                        Ok(Some(delta)) => {
                            consecutive_errors = 0;
                            on_delta(&delta);
                            full.push_str(&delta);
                        }
                        Ok(None) => consecutive_errors = 0,
                        Err(_) => {
                            consecutive_errors += 1;
                            if consecutive_errors >= MAX_CONSECUTIVE_PARSE_ERRORS {
                                return Err(Error::Llm(format!(
                                    "连续 {consecutive_errors} 个 SSE chunk 解析失败，放弃"
                                )));
                            }
                        }
                    },
                }
            }
        }

        Ok((full, record))
    }

    /// 设置页「测试连接」：区分鉴权失败 / 模型名错误 / 网络不通。

    /// 列出端点上已装好的模型。
    ///
    /// 走 OpenAI 兼容的 `/models`，Ollama、vLLM、llama.cpp server 都实现了它。
    /// 用于设置页把「模型」从一个手打输入框变成一份真实清单——
    /// 打错模型名会得到一个 404，而 404 长得跟「服务没起来」很像，很难排查。
    pub async fn list_models(&self) -> Result<Vec<String>> {
        policy::check_endpoint(self.policy, &self.config.api_base)?;
        if self.is_gemini() {
            return self.list_gemini_models().await;
        }
        let base = self.config.api_base.trim().trim_end_matches('/');
        let base = base
            .strip_suffix("/chat/completions")
            .or_else(|| base.strip_suffix("/responses"))
            .unwrap_or(base);
        let url = format!("{base}/models");
        let resp = self
            .apply_headers(self.http.get(&url), false)
            .send()
            .await
            .map_err(|e| Error::Llm(format!("连接 {url} 失败：{e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Llm(format!("{url} 返回 HTTP {}", resp.status())));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Llm(format!("解析模型列表失败：{e}")))?;
        let mut names: Vec<String> = json
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }

    /// Gemini 的模型清单：`GET {root}/models`，名字在 `models[].name`，形如 `models/gemini-3.8-flash`。
    async fn list_gemini_models(&self) -> Result<Vec<String>> {
        let root = gemini_root(self.config.api_base.trim().trim_end_matches('/'));
        let url = format!("{root}/models");
        let resp = self
            .apply_headers(self.http.get(&url), false)
            .send()
            .await
            .map_err(|e| Error::Llm(format!("连接 {url} 失败：{e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Llm(format!("{url} 返回 HTTP {}", resp.status())));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Llm(format!("解析模型列表失败：{e}")))?;
        let mut names: Vec<String> = json
            .get("models")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                    .map(|s| s.strip_prefix("models/").unwrap_or(s).to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }


    /// Ollama 的根地址：把 OpenAI 兼容前缀 `/v1` 去掉。
    ///
    /// 拉模型不是 OpenAI 协议的一部分，只能走 Ollama 自己的 `/api/pull`。
    fn ollama_root(&self) -> String {
        let base = self.config.api_base.trim_end_matches('/');
        base.strip_suffix("/v1").unwrap_or(base).to_string()
    }

    /// 让端点自己去拉一个模型（仅 Ollama）。
    ///
    /// 这是「一键配置」的最后一环：识别模型能一键下，会议总结LLM不能的话，
    /// 用户还是得去开命令行。不是 Ollama 的端点会返回 404，照实说就行。
    pub async fn pull_model(
        &self,
        model: &str,
        on_progress: &dyn Fn(PullProgress),
    ) -> Result<()> {
        policy::check_endpoint(self.policy, &self.config.api_base)?;
        let url = format!("{}/api/pull", self.ollama_root());
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "model": model, "stream": true }))
            .send()
            .await
            .map_err(|e| Error::Llm(format!("连接 {url} 失败：{e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::Llm(format!(
                "{url} 返回 404。拉取模型只有 Ollama 支持；\
                 其他服务请自行准备好模型再回来选。"
            )));
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Llm(format!(
                "拉取失败：HTTP {status} {}",
                truncate(&body, 300)
            )));
        }

        // NDJSON：一行一个 JSON 对象，不是 SSE。
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut last_status = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Llm(format!("拉取中断：{e}")))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
                    return Err(Error::Llm(format!("拉取失败：{err}")));
                }
                let status = json
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let completed = json.get("completed").and_then(|c| c.as_u64());
                let total = json.get("total").and_then(|t| t.as_u64());
                if status != last_status || completed.is_some() {
                    last_status = status.clone();
                    on_progress(PullProgress {
                        status,
                        completed,
                        total,
                    });
                }
            }
        }
        Ok(())
    }

    /// 端点体检：分清「服务没起来」「起来了但一个模型都没有」「模型名不对」。
    ///
    /// 这三种情况在 `test_connection` 里都表现为一次失败，但用户要做的事完全不同：
    /// 分别是启动服务、拉模型、改名字。不分开说，用户只能猜。
    pub async fn diagnose(&self) -> LlmDiagnosis {
        let endpoint = self.config.api_base.clone();
        let wanted = self.config.model.clone();
        let is_local = policy::is_local_endpoint(&endpoint);

        let installed = match self.list_models().await {
            Ok(v) => v,
            Err(Error::EgressBlocked { reason, .. }) => {
                return LlmDiagnosis {
                    reachable: false,
                    installed: Vec::new(),
                    model_present: false,
                    endpoint,
                    wanted_model: wanted,
                    message: reason,
                    hint: Some("当前策略限制仅允许本地端点，请在上方出网策略切换为「允许任意端点」。".into()),
                };
            }
            Err(e) => {
                let err_msg = e.to_string();
                // 鉴权失败：HTTP 401 / 403
                if err_msg.contains("401") || err_msg.contains("403") {
                    return LlmDiagnosis {
                        reachable: false,
                        installed: Vec::new(),
                        model_present: false,
                        endpoint,
                        wanted_model: wanted,
                        message: format!("鉴权失败：{err_msg}"),
                        hint: Some("请在下方「API 密钥」填入有效 Token 并点击保存。".into()),
                    };
                }

                // 远端端点若 /models 404 或不支持，尝试走一次 ping 验证模型可用性
                if !is_local {
                    let ping = [ChatMessage::user("ping")];
                    if self.complete(&ping, "diagnose_ping", false).await.is_ok() {
                        return LlmDiagnosis {
                            reachable: true,
                            installed: vec![wanted.clone()],
                            model_present: true,
                            message: "就绪，远端模型已通过调用测试".into(),
                            hint: None,
                            endpoint,
                            wanted_model: wanted,
                        };
                    }
                }

                let hint = if is_local {
                    Some("确认服务已启动。本机 Ollama 的话跑 `ollama serve`。".into())
                } else {
                    Some("确认远端服务地址正确、网络畅通，且已正确保存 API 密钥。".into())
                };

                return LlmDiagnosis {
                    reachable: false,
                    installed: Vec::new(),
                    model_present: false,
                    endpoint,
                    wanted_model: wanted,
                    message: format!("连不上：{e}"),
                    hint,
                };
            }
        };

        if installed.is_empty() {
            let (message, hint) = if is_local {
                ("服务在跑，但一个模型都没装".into(), Some(format!("拉一个：`ollama pull {wanted}`")))
            } else {
                ("远端服务在跑，但未列出任何模型".into(), Some("可尝试点击「测试连接」直接验证模型可用性。".into()))
            };
            return LlmDiagnosis {
                reachable: true,
                installed,
                model_present: false,
                endpoint,
                wanted_model: wanted,
                message,
                hint,
            };
        }

        // 支持宽松匹配：
        // 1. 完全一致
        // 2. 忽略大小写
        // 3. Ollama tag 匹配 (qwen2.5:7b -> qwen2.5)
        // 4. Gemini 等带 models/ 前缀匹配 (models/gemini-2.0-flash <-> gemini-2.0-flash)
        let present = installed.iter().any(|m| {
            m == &wanted
                || m.eq_ignore_ascii_case(&wanted)
                || m.split(':').next() == Some(wanted.as_str())
                || m.strip_prefix("models/") == Some(wanted.as_str())
                || wanted.strip_prefix("models/") == Some(m.as_str())
        });

        if present {
            LlmDiagnosis {
                reachable: true,
                model_present: true,
                message: format!("就绪，{} 个模型可用", installed.len()),
                hint: None,
                installed,
                endpoint,
                wanted_model: wanted,
            }
        } else {
            // 如果列表未精准匹配，但在远端情况下尝试一次 ping
            if !is_local {
                let ping = [ChatMessage::user("ping")];
                if self.complete(&ping, "diagnose_ping", false).await.is_ok() {
                    return LlmDiagnosis {
                        reachable: true,
                        model_present: true,
                        message: format!("就绪，远端模型响应正常（已检测到 {} 个模型）", installed.len()),
                        hint: None,
                        installed,
                        endpoint,
                        wanted_model: wanted,
                    };
                }
            }

            let hint = if is_local {
                Some(format!(
                    "换成已装的（{}），或者跑 `ollama pull {wanted}`",
                    installed.join("、")
                ))
            } else {
                Some(format!(
                    "请检查模型名拼写是否正确，或换用检测到的可用模型（{}）",
                    installed.join("、")
                ))
            };

            LlmDiagnosis {
                reachable: true,
                model_present: false,
                message: format!("服务在跑，但没有 `{wanted}`"),
                hint,
                installed,
                endpoint,
                wanted_model: wanted,
            }
        }
    }

    pub async fn test_connection(&self) -> Result<String> {
        let messages = [ChatMessage::user("ping")];
        match self.complete(&messages, "connection_test", false).await {
            Ok((text, _)) => Ok(format!("连接正常，模型返回 {} 字符", text.chars().count())),
            Err(Error::Llm(msg)) if msg.contains("401") || msg.contains("403") => {
                Err(Error::Llm(format!("鉴权失败，请检查 API Key：{msg}")))
            }
            Err(Error::Llm(msg)) if msg.contains("404") => Err(Error::Llm(format!(
                "端点或模型名不存在，请检查 api_base 与 model：{msg}"
            ))),
            Err(e) => Err(e),
        }
    }
}

/// Ollama 拉取模型的进度。`total` 只在下载 layer 的阶段才有。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PullProgress {
    /// Ollama 自己给的阶段文本，如 `pulling manifest`、`verifying sha256 digest`。
    pub status: String,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

/// LLM 端点的体检结论。UI 据此决定说哪句话、给哪个按钮。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmDiagnosis {
    /// 端点能连上（不代表模型就位）。
    pub reachable: bool,
    /// 端点上已装的模型名。
    pub installed: Vec<String>,
    /// 配置里那个模型在不在。
    pub model_present: bool,
    pub endpoint: String,
    pub wanted_model: String,
    /// 一句话结论。
    pub message: String,
    /// 下一步该干什么。已经就绪时为 None。
    pub hint: Option<String>,
}

fn should_retry_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// 非流式响应里的正文。Chat 取 `choices[0].message.content`，
/// Responses 取 `output_text`，没有就从 `output[].content[]` 拼。
fn extract_completion_text(json: &serde_json::Value) -> Option<String> {
    if json.get("candidates").is_some() {
        return gemini_text(json);
    }
    if let Some(text) = json.get("output_text").and_then(|t| t.as_str()) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    if let Some(parts) = json.get("output").and_then(|o| o.as_array()) {
        let mut buf = String::new();
        for part in parts {
            let Some(contents) = part.get("content").and_then(|c| c.as_array()) else {
                continue;
            };
            for content in contents {
                if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                    buf.push_str(text);
                }
            }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    json.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Gemini 正文在 `candidates[0].content.parts[].text`。
fn gemini_text(json: &serde_json::Value) -> Option<String> {
    let parts = json
        .get("candidates")?
        .get(0)?
        .get("content")?
        .get("parts")?
        .as_array()?;
    let mut buf = String::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            buf.push_str(text);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// 服务地址收成 Gemini 的 API 根。`.../v1beta`、`.../v1beta/openai`、
/// 甚至已经写到方法路径，都归一到 `https://host/v1beta`。
fn gemini_root(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if let Some(i) = base.find("/v1beta") {
        base[..=i + "/v1beta".len() - 1].to_string()
    } else {
        format!("{base}/v1beta")
    }
}

fn gemini_model_id(model: &str) -> String {
    let model = model.trim().trim_matches('/');
    model
        .strip_prefix("models/")
        .unwrap_or(model)
        .to_string()
}

/// 从一个 SSE data payload 里取出增量文本。
///
/// Chat 读 `choices[0].delta.content`，Responses 读 `delta`（文本事件）
/// 或 `response.output_text.delta`。`Ok(None)` 表示合法但无内容的事件。
fn extract_stream_delta(payload: &str) -> std::result::Result<Option<String>, serde_json::Error> {
    let json: serde_json::Value = serde_json::from_str(payload)?;
    if json.get("candidates").is_some() {
        return Ok(gemini_text(&json));
    }
    if let Some(kind) = json.get("type").and_then(|t| t.as_str()) {
        if kind == "response.output_text.delta" {
            return Ok(json
                .get("delta")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string));
        }
        if kind.starts_with("response.") {
            return Ok(None);
        }
    }
    extract_delta(&json)
}

fn extract_delta(json: &serde_json::Value) -> std::result::Result<Option<String>, serde_json::Error> {
    let Some(choices) = json.get("choices").and_then(|c| c.as_array()) else {
        return Ok(None);
    };
    let Some(first) = choices.first() else {
        // 空数组：某些网关的 usage-only chunk。合法，不是错误。
        return Ok(None);
    };
    let delta = first
        .get("delta")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str());
    Ok(delta.filter(|s| !s.is_empty()).map(str::to_string))
}

#[derive(Debug)]
pub enum SseEvent {
    Data(String),
    Done,
}

/// 增量 SSE 解析器。处理跨 chunk 截断的半行、`\r\n`、注释行与多行 data。
#[derive(Default)]
pub struct SseParser {
    buf: String,
    data_lines: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut events = Vec::new();

        // 只处理完整行；最后一段没有换行的留在缓冲里等下一个 chunk。
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);

            if line.is_empty() {
                // 空行 = 事件边界
                if !self.data_lines.is_empty() {
                    let payload = self.data_lines.join("\n");
                    self.data_lines.clear();
                    if payload.trim() == "[DONE]" {
                        events.push(SseEvent::Done);
                    } else {
                        events.push(SseEvent::Data(payload));
                    }
                }
                continue;
            }

            if line.starts_with(':') {
                // 注释行（部分网关用它做 keep-alive）
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                self.data_lines
                    .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // 其余字段（event:/id:/retry:）MVP 用不到。
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payloads(events: Vec<SseEvent>) -> Vec<String> {
        events
            .into_iter()
            .map(|e| match e {
                SseEvent::Data(d) => d,
                SseEvent::Done => "[DONE]".to_string(),
            })
            .collect()
    }

    #[test]
    fn parses_simple_events() {
        let mut p = SseParser::default();
        let ev = p.push(b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(payloads(ev), vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn handles_line_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"hel").is_empty());
        assert!(p.push(b"lo\":1}").is_empty(), "无换行不应产生事件");
        let ev = p.push(b"\n\n");
        assert_eq!(payloads(ev), vec!["{\"hello\":1}"]);
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut p = SseParser::default();
        let ev = p.push(b": keep-alive\r\ndata: {\"x\":1}\r\n\r\n");
        assert_eq!(payloads(ev), vec!["{\"x\":1}"]);
    }

    #[test]
    fn joins_multiline_data() {
        let mut p = SseParser::default();
        let ev = p.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(payloads(ev), vec!["line1\nline2"]);
    }

    #[test]
    fn empty_choices_array_is_not_an_error() {
        // 真实网关的 usage-only chunk
        let out = extract_stream_delta(r#"{"choices":[],"usage":{"total_tokens":10}}"#).unwrap();
        assert_eq!(out, None, "空 choices 必须跳过而不是 panic");
    }

    #[test]
    fn role_chunk_yields_no_delta() {
        let out =
            extract_stream_delta(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn content_delta_is_extracted() {
        let out = extract_stream_delta(r#"{"choices":[{"delta":{"content":"你好"}}]}"#).unwrap();
        assert_eq!(out.as_deref(), Some("你好"));
    }

    #[test]
    fn responses_delta_is_extracted() {
        let out = extract_stream_delta(
            r#"{"type":"response.output_text.delta","delta":"你好"}"#,
        )
        .unwrap();
        assert_eq!(out.as_deref(), Some("你好"));
        let skip = extract_stream_delta(r#"{"type":"response.completed"}"#).unwrap();
        assert_eq!(skip, None);
    }

    #[test]
    fn responses_completion_text() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"output_text":"纪要","output":[{"content":[{"text":"ignored"}]}]}"#,
        )
        .unwrap();
        assert_eq!(extract_completion_text(&json).as_deref(), Some("纪要"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(extract_stream_delta("not json").is_err());
    }

    #[test]
    fn retry_policy_only_covers_429_and_5xx() {
        assert!(should_retry_status(429));
        assert!(should_retry_status(500));
        assert!(should_retry_status(503));
        assert!(!should_retry_status(400));
        assert!(!should_retry_status(401));
        assert!(!should_retry_status(404));
    }

    #[test]
    fn local_only_policy_blocks_client_before_any_request() {
        let cfg = LlmConfig {
            api_base: "https://api.openai.com/v1".into(),
            ..LlmConfig::default()
        };
        let client = LlmClient::new(cfg, EgressPolicy::LocalOnly, None).unwrap();
        let err = client
            .guard(&[ChatMessage::user("hi")], "test", false)
            .unwrap_err();
        assert!(matches!(err, Error::EgressBlocked { .. }));
    }

    #[test]
    fn endpoint_normalization_handles_various_formats() {
        let cases = [
            ("http://localhost:11434/v1", "http://localhost:11434/v1/chat/completions"),
            ("http://localhost:11434/v1/", "http://localhost:11434/v1/chat/completions"),
            ("https://api.openai.com/v1/responses", "https://api.openai.com/v1/responses"),
        ];

        for (input, expected) in cases {
            let cfg = LlmConfig {
                api_base: input.into(),
                api_format: if input.ends_with("/responses") {
                    "responses".into()
                } else {
                    "chat".into()
                },
                ..LlmConfig::default()
            };
            let client = LlmClient::new(cfg, EgressPolicy::Open, None).unwrap();
            assert_eq!(client.endpoint(false), expected, "failed for input: {input}");
        }
    }

    #[test]
    fn gemini_uses_native_generate_content() {
        let cases = [
            "https://generativelanguage.googleapis.com/v1beta",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "https://generativelanguage.googleapis.com",
        ];
        for input in cases {
            let cfg = LlmConfig {
                api_base: input.into(),
                provider: "gemini".into(),
                model: "gemini-3.8-flash".into(),
                ..LlmConfig::default()
            };
            let client = LlmClient::new(cfg, EgressPolicy::Open, Some("k".into())).unwrap();
            assert_eq!(
                client.endpoint(false),
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
            );
            assert!(client.endpoint(true).contains(":streamGenerateContent?alt=sse"));
        }
        let body = {
            let cfg = LlmConfig {
                provider: "gemini".into(),
                model: "gemini-3.8-flash".into(),
                ..LlmConfig::default()
            };
            let client = LlmClient::new(cfg, EgressPolicy::Open, None).unwrap();
            client.build_body(
                &[ChatMessage::system("规则"), ChatMessage::user("你好")],
                false,
            )
        };
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "你好");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "规则");
        assert!(body.get("messages").is_none());
    }

    #[test]
    fn gemini_text_comes_from_candidates() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"纪要"}]}}]}"#,
        )
        .unwrap();
        assert_eq!(extract_completion_text(&json).as_deref(), Some("纪要"));
    }

    #[test]
    fn local_endpoint_detection_differentiates_cloud_and_local() {
        assert!(policy::is_local_endpoint("http://localhost:11434/v1"));
        assert!(policy::is_local_endpoint("http://127.0.0.1:8000"));
        assert!(policy::is_local_endpoint("http://[::1]:11434"));
        assert!(!policy::is_local_endpoint("https://generativelanguage.googleapis.com/v1beta/openai"));
        assert!(!policy::is_local_endpoint("https://api.deepseek.com/v1"));
        assert!(!policy::is_local_endpoint("https://api.openai.com/v1"));
    }

    #[test]
    fn audit_record_counts_chars_but_not_content() {
        let client = LlmClient::new(LlmConfig::default(), EgressPolicy::LocalOnly, None).unwrap();
        let msgs = [ChatMessage::system("abc"), ChatMessage::user("你好呀")];
        let rec = client.guard(&msgs, "summarize", true).unwrap();
        assert_eq!(rec.chars_sent, 6);
        assert!(rec.redacted);
        assert_eq!(rec.policy, "local_only");
        // 审计记录里不该出现内容本身
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("你好呀"));
    }
}
