//! 应用配置。落盘为 JSON，位于应用数据目录；**不含任何密钥**——
//! API Key 与数据库主密钥都在 OS Keychain 里（§5）。

use std::path::{Path, PathBuf};

use crate::asr::EngineOptions;
use crate::policy::EgressPolicy;
use crate::{Error, Result};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// 应用数据根目录：数据库、音频分片、日志。
    pub data_dir: PathBuf,
    /// ONNX 权重目录。
    pub models_dir: PathBuf,
    /// 提示词模板目录。
    pub templates_dir: PathBuf,
    pub egress_policy: EgressPolicy,
    pub llm: LlmConfig,
    pub engine: EngineConfig,
    pub capture: CaptureConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// OpenAI 兼容端点，形如 `http://localhost:11434/v1`。
    pub api_base: String,
    pub model: String,
    /// 上下文窗口（token）。决定 map-reduce 的 chunk 大小，见 §4.4。
    pub context_tokens: usize,
    pub temperature: f32,
    pub max_tokens: u32,
    /// 首字节超时（秒）。
    pub connect_timeout_secs: u64,
    /// 整体超时（秒）。
    pub request_timeout_secs: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            // 默认指向本地 Ollama——与 LocalOnly 策略自洽，开箱不出网。
            api_base: "http://localhost:11434/v1".to_string(),
            model: "qwen2.5:7b".to_string(),
            context_tokens: 8192,
            temperature: 0.2,
            max_tokens: 4096,
            connect_timeout_secs: 30,
            request_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub num_threads: i32,
    pub provider: String,
    pub cluster_threshold: f32,
    pub merge_gap_ms: u32,
    pub max_speech_duration: f32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let d = EngineOptions::default();
        Self {
            num_threads: d.num_threads,
            provider: d.provider,
            cluster_threshold: d.cluster_threshold,
            merge_gap_ms: d.merge_gap_ms,
            max_speech_duration: d.max_speech_duration,
        }
    }
}

impl EngineConfig {
    pub fn to_options(&self) -> EngineOptions {
        EngineOptions {
            num_threads: self.num_threads,
            provider: self.provider.clone(),
            cluster_threshold: self.cluster_threshold,
            merge_gap_ms: self.merge_gap_ms,
            max_speech_duration: self.max_speech_duration,
            ..EngineOptions::default()
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// 分片时长（秒）。§4.1：崩溃最多损失一个分片。
    pub chunk_seconds: u32,
    /// 磁盘剩余低于此值（字节）时告警并停止录制。
    pub min_free_bytes: u64,
    pub record_mic: bool,
    pub record_system: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            chunk_seconds: 60,
            min_free_bytes: 2 * 1024 * 1024 * 1024, // 2GB
            record_mic: true,
            record_system: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = default_data_dir();
        Self {
            // 绝对路径，不是 "models"。相对路径会跟着进程的工作目录跑：
            // 开发时 cwd 是仓库根，装完之后是快捷方式指向的任意目录，
            // 于是「模型明明在盘上却报 not found」。见 §6。
            models_dir: data_dir.join("models"),
            templates_dir: data_dir.join("templates"),
            egress_policy: EgressPolicy::default(),
            llm: LlmConfig::default(),
            engine: EngineConfig::default(),
            capture: CaptureConfig::default(),
            data_dir,
        }
    }
}

/// 配置文件的固定位置：数据目录下。
///
/// 不放当前目录——装到 `Program Files` 之后那里根本写不进去，
/// 而且每次从不同地方启动会读到不同的配置。
pub fn default_config_path() -> PathBuf {
    default_data_dir().join("vocmeet.config.json")
}

fn default_data_dir() -> PathBuf {
    // Windows: %LOCALAPPDATA%\VocMeet；其他平台回落到 ~/.vocmeet。
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local).join("VocMeet");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".vocmeet");
    }
    PathBuf::from(".vocmeet")
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let mut cfg: Self = serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg.absolutize();
        Ok(cfg)
    }

    /// 把配置里的相对路径钉成绝对路径。
    ///
    /// 老配置文件里可能存着 `"models"` 这种相对值（也可能是用户手写的）。
    /// 相对路径的解析基准是进程工作目录，而工作目录不受我们控制，
    /// 所以在这里一次性钉死：先认工作目录下的，其次认数据目录下的。
    pub fn absolutize(&mut self) {
        let data_dir = self.data_dir.clone();
        for (field, fallback) in [
            (&mut self.models_dir, "models"),
            (&mut self.templates_dir, "templates"),
        ] {
            if field.is_absolute() {
                continue;
            }
            let from_cwd = std::env::current_dir().ok().map(|d| d.join(&*field));
            *field = match from_cwd {
                // 工作目录下确实有，说明是开发时的仓库布局，保留它。
                Some(p) if p.exists() => p,
                _ => data_dir.join(fallback),
            };
        }
        if !self.data_dir.is_absolute() {
            if let Ok(cwd) = std::env::current_dir() {
                self.data_dir = cwd.join(&self.data_dir);
            }
        }
    }

    /// 首次启动时，把权重目录指到实际存在的那一份。
    ///
    /// 三个候选按优先级：当前配置值（用户显式设过就听用户的）、可执行文件旁边
    /// （绿色包解压出来的布局）、数据目录（一键下载的落点）。
    /// 都没有就保持默认，UI 上会显示「缺失」并给出下载按钮。
    pub fn adopt_existing_models(&mut self, exe_dir: Option<&Path>) {
        if ModelsProbe::looks_populated(&self.models_dir) {
            return;
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(d) = exe_dir {
            candidates.push(d.join("models"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("models"));
        }
        candidates.push(self.data_dir.join("models"));
        if let Some(found) = candidates.into_iter().find(|c| ModelsProbe::looks_populated(c)) {
            self.models_dir = found;
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize config: {e}")))?;
        std::fs::write(path, raw).map_err(|e| Error::io(path, e))
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("vocmeet.db")
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.data_dir.join("audio")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [&self.data_dir, &self.audio_dir()] {
            std::fs::create_dir_all(d).map_err(|e| Error::io(d, e))?;
        }
        Ok(())
    }
}

/// 「这个目录里像不像有一套权重」的轻量判断。
///
/// 只看标志性文件在不在，不算 SHA256——启动路径上不该为了挑目录读 360MB。
struct ModelsProbe;

impl ModelsProbe {
    fn looks_populated(dir: &Path) -> bool {
        dir.join("silero_vad.onnx").is_file() && dir.join("speaker-embedding.onnx").is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_are_absolute() {
        let c = Config::default();
        // 相对路径会跟着工作目录跑，装完就找不到模型——这条守着那个 bug。
        assert!(c.models_dir.is_absolute(), "models_dir 必须是绝对路径");
        assert!(c.templates_dir.is_absolute(), "templates_dir 必须是绝对路径");
        assert!(default_config_path().is_absolute());
    }

    #[test]
    fn absolutize_pins_legacy_relative_paths() {
        let mut c = Config::default();
        c.models_dir = PathBuf::from("models");
        c.templates_dir = PathBuf::from("templates");
        c.absolutize();
        assert!(c.models_dir.is_absolute());
        assert!(c.templates_dir.is_absolute());
    }

    #[test]
    fn adopt_existing_models_prefers_a_populated_dir() {
        let base = std::env::temp_dir().join(format!("vocmeet_adopt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let beside_exe = base.join("app").join("models");
        std::fs::create_dir_all(&beside_exe).unwrap();
        std::fs::write(beside_exe.join("silero_vad.onnx"), b"x").unwrap();
        std::fs::write(beside_exe.join("speaker-embedding.onnx"), b"x").unwrap();

        let mut c = Config::default();
        c.data_dir = base.join("data");
        c.models_dir = base.join("data").join("models"); // 空的
        c.adopt_existing_models(Some(&base.join("app")));
        assert_eq!(c.models_dir, beside_exe, "应当认出可执行文件旁边那一套");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn adopt_keeps_current_dir_when_already_populated() {
        let base = std::env::temp_dir().join(format!("vocmeet_keep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mine = base.join("mine");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("silero_vad.onnx"), b"x").unwrap();
        std::fs::write(mine.join("speaker-embedding.onnx"), b"x").unwrap();

        let mut c = Config::default();
        c.models_dir = mine.clone();
        c.adopt_existing_models(None);
        assert_eq!(c.models_dir, mine, "用户设过的目录不该被顶掉");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn defaults_are_offline_first() {
        let c = Config::default();
        assert_eq!(c.egress_policy, EgressPolicy::LocalOnly);
        assert!(
            c.llm.api_base.contains("localhost"),
            "默认端点必须是本机，否则开箱即出网"
        );
    }

    #[test]
    fn config_roundtrips_through_json() {
        let dir = std::env::temp_dir().join(format!("vocmeet_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");

        let mut c = Config::default();
        c.llm.model = "qwen3:8b".into();
        c.engine.num_threads = 8;
        c.save(&p).unwrap();

        let back = Config::load(&p).unwrap();
        assert_eq!(back.llm.model, "qwen3:8b");
        assert_eq!(back.engine.num_threads, 8);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_config_falls_back_to_defaults() {
        let c = Config::load("no-such-config-file.json").unwrap();
        assert_eq!(c.egress_policy, EgressPolicy::LocalOnly);
    }
}
