use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("audio error: {0}")]
    Audio(String),

    /// sherpa-onnx 的 C API 大量返回 `Option`/空指针而非结构化错误，
    /// 因此这里统一转成带上下文的字符串，避免上层拿到一个裸 `None`。
    #[error("inference engine error: {0}")]
    Engine(String),

    #[error("model `{name}` not found at {path}; run `vocmeet-cli fetch-models` first")]
    ModelMissing { name: String, path: PathBuf },

    #[error("model `{name}` failed checksum: expected {expected}, got {actual}")]
    ModelChecksum {
        name: String,
        expected: String,
        actual: String,
    },

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("keychain error: {0}")]
    Keychain(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    /// 出网守卫拒绝。见 §5：这是策略拒绝，不是网络故障，UI 需要区别提示。
    #[error("egress blocked by policy `{policy}`: {reason}")]
    EgressBlocked { policy: String, reason: String },

    #[error("llm error: {0}")]
    Llm(String),

    #[error("template error: {0}")]
    Template(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("job cancelled")]
    Cancelled,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn engine(msg: impl Into<String>) -> Self {
        Error::Engine(msg.into())
    }

    pub fn audio(msg: impl Into<String>) -> Self {
        Error::Audio(msg.into())
    }

    /// 用户取消与真实失败在 UI 上表现不同，任务队列据此决定是否记为错误。
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled)
    }
}
