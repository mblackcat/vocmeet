//! ONNX 权重的路径解析与完整性校验。
//!
//! 权重不入库。取得权重有两条路：`scripts/fetch-models.sh`（CI 与无 GUI 场景），
//! 以及应用内的一键下载 [`fetch_all`]。两条路的资产清单必须保持一致。
//!
//! 本模块做三件事：
//! 1. 把「模型集目录」解析成各个具体文件路径；
//! 2. 启动时校验文件存在与 SHA256，损坏时给出可操作的修复指引（§6）；
//! 3. 按清单下载、解压、校验（§6「首次启动」）。

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// 可选的识别模型。VAD / 声纹 / 标点三套是共用的，只有 ASR 这一份可换。
///
///
/// 不实现 `Default`：新装与升级的默认值不同，见 [`crate::config::EngineConfig`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrModel {
    /// SenseVoice-Small int8：最小最快，中文准确率一般。
    SenseVoice,
    /// FireRedASR2 AED int8：中文明显更准，ASR 耗时约为 SenseVoice 的 6~7 倍。
    ///
    /// 同期还有个 FireRedASR2 CTC 版，实测（57s 四人中文 + 七段方言/中英混说）
    /// 准确率与 SenseVoice 互有胜负、却慢 5 倍多，所以没收。
    FireRedAed,
}

/// 一个识别模型的全部静态信息：目录、文件名、下载地址、期望校验和。
struct AsrSpec {
    dir: &'static str,
    /// SenseVoice 是单个模型文件；AED 是 encoder。
    model_file: &'static str,
    /// 仅 AED 有。
    decoder_file: Option<&'static str>,
    asset: Asset,
    /// 解压后文件的 SHA256，按 `entries()` 的名字对应。
    sha256: &'static [(&'static str, &'static str)],
}

impl AsrModel {
    pub const ALL: [AsrModel; 2] = [AsrModel::SenseVoice, AsrModel::FireRedAed];

    fn spec(self) -> &'static AsrSpec {
        match self {
            AsrModel::SenseVoice => &SENSE_VOICE,
            AsrModel::FireRedAed => &FIRE_RED_AED,
        }
    }

    pub fn label(self) -> &'static str {
        self.spec().asset.label
    }

    /// 这个识别模型的压缩包体积。
    pub fn download_bytes(self) -> u64 {
        self.spec().asset.approx_bytes
    }
}

/// 一套完整的推理权重。
#[derive(Debug, Clone)]
pub struct ModelSet {
    pub root: PathBuf,
    pub asr: AsrModel,
    /// SenseVoice 的模型文件，或 FireRed AED 的 encoder。
    pub asr_model: PathBuf,
    /// FireRed AED 的 decoder；其余模型为 None。
    pub asr_decoder: Option<PathBuf>,
    pub asr_tokens: PathBuf,
    /// Silero VAD。
    pub vad_model: PathBuf,
    /// pyannote segmentation 3.0（MIT）。
    pub segmentation_model: PathBuf,
    /// 3D-Speaker eres2net 声纹 embedding（Apache-2.0）。
    pub embedding_model: PathBuf,
    /// CT-Transformer 中英标点恢复 int8。
    pub punctuation_model: PathBuf,
}

const SENSE_VOICE_DIR: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09";
const FIRE_RED_AED_DIR: &str = "sherpa-onnx-fire-red-asr2-zh_en-int8-2026-02-26";
const PYANNOTE_DIR: &str = "sherpa-onnx-pyannote-segmentation-3-0";
const PUNCT_DIR: &str = "sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8";

impl ModelSet {
    /// 按 `scripts/fetch-models.sh` 的目录约定解析路径。
    pub fn from_root(root: impl Into<PathBuf>, asr: AsrModel) -> Self {
        let root = root.into();
        let spec = asr.spec();
        let asr_dir = root.join(spec.dir);
        Self {
            asr,
            asr_model: asr_dir.join(spec.model_file),
            asr_decoder: spec.decoder_file.map(|f| asr_dir.join(f)),
            asr_tokens: asr_dir.join("tokens.txt"),
            vad_model: root.join("silero_vad.onnx"),
            segmentation_model: root.join(PYANNOTE_DIR).join("model.onnx"),
            embedding_model: root.join("speaker-embedding.onnx"),
            punctuation_model: root.join(PUNCT_DIR).join("model.int8.onnx"),
            root,
        }
    }

    fn entries(&self) -> Vec<(&'static str, &Path)> {
        let mut out = vec![("asr_model", self.asr_model.as_path())];
        if let Some(d) = &self.asr_decoder {
            out.push(("asr_decoder", d.as_path()));
        }
        out.extend([
            ("asr_tokens", self.asr_tokens.as_path()),
            ("vad_model", &self.vad_model),
            ("segmentation_model", &self.segmentation_model),
            ("embedding_model", &self.embedding_model),
            ("punctuation_model", &self.punctuation_model),
        ]);
        out
    }

    /// 这套权重每个文件的期望 SHA256：识别模型自己的 + 共用的。
    pub fn expected_sha256(&self) -> Vec<(String, String)> {
        self.asr
            .spec()
            .sha256
            .iter()
            .chain(SHARED_SHA256)
            .map(|(n, h)| (n.to_string(), h.to_string()))
            .collect()
    }

    /// 校验全部权重存在。缺失时报出具体是哪个模型、该跑什么命令修。
    pub fn verify_present(&self) -> Result<()> {
        for (name, path) in self.entries() {
            if !path.exists() {
                return Err(Error::ModelMissing {
                    name: name.to_string(),
                    path: path.to_path_buf(),
                });
            }
        }
        Ok(())
    }

    /// 逐个文件算 SHA256。用于 `doctor` 子命令与首启自检（§6 模型损坏）。
    pub fn checksums(&self) -> Result<Vec<(String, String, u64)>> {
        let mut out = Vec::new();
        for (name, path) in self.entries() {
            let (digest, size) = sha256_file(path)?;
            out.push((name.to_string(), digest, size));
        }
        Ok(out)
    }

    /// 对照期望校验和。`expected` 为 (名称, sha256) 列表，通常来自随包分发的清单。
    pub fn verify_checksums(&self, expected: &[(String, String)]) -> Result<()> {
        let actual = self.checksums()?;
        for (name, want) in expected {
            let Some((_, got, _)) = actual.iter().find(|(n, _, _)| n == name) else {
                continue;
            };
            if got != want {
                return Err(Error::ModelChecksum {
                    name: name.clone(),
                    expected: want.clone(),
                    actual: got.clone(),
                });
            }
        }
        Ok(())
    }

    /// 全部权重占用的磁盘字节数，用于 UI 展示与安装包体积核算（§7）。
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0;
        for (_, path) in self.entries() {
            let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
            total += meta.len();
        }
        Ok(total)
    }
}


// ---------------------------------------------------------------- 一键下载（§6）

/// 一份待下载的资产。
///
/// `sha256` 是**解压后最终文件**的校验和，不是压缩包的——校验的目标是
/// 「这台机器上能不能推理」，压缩包对不对只是中间态。
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// UI 上显示的名字。
    pub label: &'static str,
    pub url: &'static str,
    pub kind: AssetKind,
    /// 压缩后的下载体积（`Content-Length` 实测值，不是解压后的大小）。
    /// 进度条按它加权，「要下多少」也报它——报解压后的大小会让进度条骗人。
    pub approx_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum AssetKind {
    /// 直接落盘到 `root/<name>`。
    Plain { file: &'static str },
    /// tar.bz2，解压到 `root/`，解压后应当出现 `dir`。
    TarBz2 { dir: &'static str },
}

static SENSE_VOICE: AsrSpec = AsrSpec {
    dir: SENSE_VOICE_DIR,
    model_file: "model.int8.onnx",
    decoder_file: None,
    asset: Asset {
        label: "识别模型 SenseVoice",
        url: concat!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download",
            "/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2"
        ),
        kind: AssetKind::TarBz2 {
            dir: SENSE_VOICE_DIR,
        },
        approx_bytes: 165_783_878,
    },
    sha256: &[
        (
            "asr_model",
            "12ca1a2ae7ecf3e0019ef2822307ee0b5cadc9196569e379b4c4026f8205276d",
        ),
        (
            "asr_tokens",
            "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc",
        ),
    ],
};

static FIRE_RED_AED: AsrSpec = AsrSpec {
    dir: FIRE_RED_AED_DIR,
    model_file: "encoder.int8.onnx",
    decoder_file: Some("decoder.int8.onnx"),
    asset: Asset {
        label: "识别模型 FireRedASR2-AED",
        url: concat!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download",
            "/asr-models/sherpa-onnx-fire-red-asr2-zh_en-int8-2026-02-26.tar.bz2"
        ),
        kind: AssetKind::TarBz2 {
            dir: FIRE_RED_AED_DIR,
        },
        approx_bytes: 838_589_068,
    },
    sha256: &[
        ("asr_model", "54048d66b6e8f3c80ea7ce95efe794587b0fd81d7271651d0decd3803852ae82"),
        ("asr_decoder", "b840ce7196ae4a14d05ae84bbf56082b6b61ccec5610fda907dddbcea37354ff"),
        ("asr_tokens", "1bc613de2112d257e61a349c3e72d1b1a9cf19c33d3ca954197ad2171e5ea07b"),
    ],
};

/// 与识别模型无关、每套都要的资产。
///
/// 和 `scripts/fetch-models.sh` 同源，两处必须一致：脚本是 CI 与无 GUI 场景的路径，
/// 这里是应用内一键下载的路径。改任何一处都要同步改另一处。
const SHARED_ASSETS: &[Asset] = &[
    Asset {
        label: "静音检测 Silero VAD",
        url: concat!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download",
            "/asr-models/silero_vad.onnx"
        ),
        kind: AssetKind::Plain {
            file: "silero_vad.onnx",
        },
        approx_bytes: 643_854,
    },
    Asset {
        label: "说话人分段 pyannote",
        url: concat!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download",
            "/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2"
        ),
        kind: AssetKind::TarBz2 { dir: PYANNOTE_DIR },
        approx_bytes: 6_958_444,
    },
    Asset {
        label: "声纹 3D-Speaker",
        url: concat!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download",
            "/speaker-recongition-models",
            "/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx"
        ),
        kind: AssetKind::Plain {
            file: "speaker-embedding.onnx",
        },
        approx_bytes: 39_593_761,
    },
    Asset {
        label: "标点恢复 CT-Transformer",
        url: concat!(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download",
            "/punctuation-models",
            "/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12-int8.tar.bz2"
        ),
        kind: AssetKind::TarBz2 { dir: PUNCT_DIR },
        approx_bytes: 64_717_756,
    },
];

/// 共用文件的期望 SHA256。识别模型的在各自的 [`AsrSpec`] 里。
///
/// 取自本机跑通过全链路的那一套权重（见 README 实测记录）。上游 release 是
/// 带日期的固定 tag，不会原地更新；真对不上就是下载损坏或上游动过手脚，两种都该拦。
const SHARED_SHA256: &[(&str, &str)] = &[
    (
        "vad_model",
        "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
    ),
    (
        "segmentation_model",
        "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
    ),
    (
        "embedding_model",
        "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b",
    ),
    (
        "punctuation_model",
        "65a3fb9f5ad7bfb96bf69e0dc4481df97f6ee60513c1d94ce981ba6effd524b1",
    ),
];

/// 选定识别模型时要下载的全部资产：识别模型在前，共用的在后。
pub fn assets(asr: AsrModel) -> Vec<Asset> {
    std::iter::once(asr.spec().asset)
        .chain(SHARED_ASSETS.iter().copied())
        .collect()
}

/// 下载总体积的粗略值，用于「要下 xxx MB」这句提示。
pub fn total_download_bytes(asr: AsrModel) -> u64 {
    assets(asr).iter().map(|a| a.approx_bytes).sum()
}

/// 下载阶段。解压与校验都可能耗时几十秒，不报出来用户会以为卡死。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchPhase {
    Downloading,
    Extracting,
    Verifying,
    Done,
}

#[derive(Debug, Clone)]
pub struct FetchProgress {
    pub phase: FetchPhase,
    /// 当前资产在清单里的序号，从 0 开始。
    pub index: usize,
    pub total_assets: usize,
    pub label: &'static str,
    /// 当前资产已接收字节。
    pub received: u64,
    /// 当前资产总字节，服务端没给 Content-Length 时为 None。
    pub total: Option<u64>,
    /// 全部资产合计的整体进度 0.0~1.0（按 `approx_bytes` 加权）。
    pub overall: f32,
}

/// 下载并铺开全部权重。已经就位且校验通过的会跳过。
///
/// **关于出网守卫**：这条路径刻意不经 [`crate::policy::check_endpoint`]。
/// 守卫管的是「用户数据出本机」（§5），而这里是**纯下行**——请求体是空的，
/// 不含逐字稿、不含任何会议内容。用户点了「下载模型」这个动作本身就是授权，
/// 与 LocalOnly 策略要防的静默外泄不是一回事。调用方仍应写一条审计行。
pub async fn fetch_all(
    root: &Path,
    asr: AsrModel,
    on_progress: &dyn Fn(FetchProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| Error::io(root, e))?;

    let set = ModelSet::from_root(root, asr);
    let expected = set.expected_sha256();
    let assets = assets(asr);
    // 已经全好了就别白下一遍几百 MB。
    if set.verify_present().is_ok() && set.verify_checksums(&expected).is_ok() {
        on_progress(FetchProgress {
            phase: FetchPhase::Done,
            index: assets.len(),
            total_assets: assets.len(),
            label: "全部权重已就位",
            received: 0,
            total: None,
            overall: 1.0,
        });
        return Ok(());
    }

    let grand_total: u64 = total_download_bytes(asr).max(1);
    let mut done_bytes: u64 = 0;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("构建 HTTP 客户端失败: {e}")))?;

    for (index, asset) in assets.iter().enumerate() {
        if cancel() {
            return Err(Error::Cancelled);
        }
        // done_bytes 在循环里要改，闭包不能借它——显式传进来。
        let base = done_bytes;
        let report = |phase: FetchPhase, received: u64, total: Option<u64>| {
            on_progress(FetchProgress {
                phase,
                index,
                total_assets: assets.len(),
                label: asset.label,
                received,
                total,
                overall: ((base + received) as f32 / grand_total as f32).clamp(0.0, 1.0),
            });
        };

        if asset_is_present(root, asset) {
            report(FetchPhase::Done, asset.approx_bytes, None);
            done_bytes += asset.approx_bytes;
            continue;
        }

        // 落到 .part 再改名：中途断掉不会留下一个看着像成品的半截文件。
        let tmp = root.join(format!("_dl_{index}.part"));
        download_to(&client, asset.url, &tmp, &|got, total| {
            report(FetchPhase::Downloading, got, total);
        }, cancel)
        .await?;

        report(FetchPhase::Extracting, asset.approx_bytes, None);
        let placed = place_asset(root, asset, &tmp);
        let _ = std::fs::remove_file(&tmp);
        placed?;

        done_bytes += asset.approx_bytes;
    }

    if cancel() {
        return Err(Error::Cancelled);
    }
    on_progress(FetchProgress {
        phase: FetchPhase::Verifying,
        index: assets.len(),
        total_assets: assets.len(),
        label: "校验完整性",
        received: 0,
        total: None,
        overall: 1.0,
    });

    set.verify_present()?;
    set.verify_checksums(&expected)?;

    on_progress(FetchProgress {
        phase: FetchPhase::Done,
        index: assets.len(),
        total_assets: assets.len(),
        label: "全部权重就位",
        received: 0,
        total: None,
        overall: 1.0,
    });
    Ok(())
}

/// 这个资产是不是已经铺好了。只看存在性——完整校验留到最后统一做一次。
fn asset_is_present(root: &Path, asset: &Asset) -> bool {
    match asset.kind {
        AssetKind::Plain { file } => root.join(file).is_file(),
        AssetKind::TarBz2 { dir } => root.join(dir).is_dir(),
    }
}

async fn download_to(
    client: &reqwest::Client,
    url: &str,
    dst: &Path,
    on_bytes: &dyn Fn(u64, Option<u64>),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    use futures_util::StreamExt;
    use std::io::Write;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("下载 {url} 失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(anyhow::anyhow!(
            "下载 {url} 失败：HTTP {}",
            resp.status()
        )));
    }
    let total = resp.content_length();

    let mut file = std::fs::File::create(dst).map_err(|e| Error::io(dst, e))?;
    let mut stream = resp.bytes_stream();
    let mut got = 0u64;
    let mut last_report = 0u64;

    while let Some(chunk) = stream.next().await {
        if cancel() {
            drop(file);
            let _ = std::fs::remove_file(dst);
            return Err(Error::Cancelled);
        }
        let chunk = chunk.map_err(|e| Error::Other(anyhow::anyhow!("下载中断: {e}")))?;
        file.write_all(&chunk).map_err(|e| Error::io(dst, e))?;
        got += chunk.len() as u64;
        // 每 2MB 报一次，别把事件通道刷爆。
        if got - last_report >= 2 << 20 {
            last_report = got;
            on_bytes(got, total);
        }
    }
    file.flush().map_err(|e| Error::io(dst, e))?;
    on_bytes(got, total);
    Ok(())
}

/// 把下载下来的临时文件放到它该在的位置：直传的改名，压缩包的解开。
fn place_asset(root: &Path, asset: &Asset, tmp: &Path) -> Result<()> {
    match asset.kind {
        AssetKind::Plain { file } => {
            let dst = root.join(file);
            std::fs::rename(tmp, &dst).map_err(|e| Error::io(&dst, e))?;
            Ok(())
        }
        AssetKind::TarBz2 { dir } => {
            extract_tar_bz2(tmp, root)?;
            let expect = root.join(dir);
            if !expect.is_dir() {
                return Err(Error::Other(anyhow::anyhow!(
                    "解压后没有出现预期目录 {}",
                    expect.display()
                )));
            }
            Ok(())
        }
    }
}

/// 解 tar.bz2。纯 Rust，不依赖系统 tar。
fn extract_tar_bz2(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).map_err(|e| Error::io(archive, e))?;
    let reader = std::io::BufReader::new(file);
    let decoder = bzip2_rs::DecoderReader::new(reader);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)
        .map_err(|e| Error::Other(anyhow::anyhow!("解压 {} 失败: {e}", archive.display())))?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_fetch_script_layout() {
        let m = ModelSet::from_root("models", AsrModel::SenseVoice);
        assert!(m.asr_model.ends_with("model.int8.onnx"));
        assert!(m.asr_model.to_string_lossy().contains(SENSE_VOICE_DIR));
        assert!(m.asr_decoder.is_none());
        assert!(m.vad_model.ends_with("silero_vad.onnx"));
        assert!(m.embedding_model.ends_with("speaker-embedding.onnx"));
    }

    #[test]
    fn missing_model_reports_name_and_path() {
        let m = ModelSet::from_root("definitely-not-here", AsrModel::SenseVoice);
        let err = m.verify_present().unwrap_err();
        match err {
            Error::ModelMissing { name, .. } => assert_eq!(name, "asr_model"),
            other => panic!("unexpected error: {other}"),
        }
    }


    #[test]
    fn expected_checksums_cover_every_model_file() {
        for asr in AsrModel::ALL {
            let set = ModelSet::from_root("x", asr);
            let expected = set.expected_sha256();
            let names: Vec<&str> = set.entries().into_iter().map(|(n, _)| n).collect();
            for name in &names {
                assert!(
                    expected.iter().any(|(n, _)| n == name),
                    "{asr:?} 的 {name} 没有期望校验和，下载完就没人拦得住它损坏"
                );
            }
            assert_eq!(expected.len(), names.len(), "{asr:?} 多余或缺失的校验和条目");
        }
    }

    #[test]
    fn fire_red_aed_has_a_decoder() {
        let m = ModelSet::from_root("x", AsrModel::FireRedAed);
        assert!(m.asr_decoder.is_some());
        assert!(m.asr_model.to_string_lossy().contains(FIRE_RED_AED_DIR));
    }

    #[test]
    fn asr_model_serializes_as_snake_case() {
        let j = serde_json::to_string(&AsrModel::FireRedAed).unwrap();
        assert_eq!(j, "\"fire_red_aed\"");
    }

    #[test]
    fn assets_stay_in_sync_with_the_fetch_script() {
        // 清单有两份（脚本 + 应用内下载），必须指向同一批文件。
        // 改了一处忘了另一处，就会出现「脚本能下、按钮下不了」这种鬼故事。
        let script = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/fetch-models.sh"
        ))
        .expect("读不到 scripts/fetch-models.sh");
        let all = AsrModel::ALL.iter().map(|a| a.spec().asset).chain(SHARED_ASSETS.iter().copied());
        for asset in all {
            let tail = asset.url.rsplit('/').next().unwrap();
            assert!(
                script.contains(tail),
                "{} 在 fetch-models.sh 里找不到，两份清单已经不一致",
                tail
            );
        }
    }

    #[test]
    fn download_size_is_the_compressed_total() {
        let total = total_download_bytes(AsrModel::SenseVoice);
        // 压缩包合计约 265MiB；解压后是 359MB。报错的值多半是把解压后的填进去了。
        assert!(
            (270_000_000..290_000_000).contains(&total),
            "下载体积 {total} 不对劲"
        );
    }

    #[test]
    fn asset_presence_check_looks_at_the_right_thing() {
        let dir = std::env::temp_dir().join(format!("vocmeet_asset_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let plain = Asset {
            label: "t",
            url: "",
            kind: AssetKind::Plain { file: "a.onnx" },
            approx_bytes: 1,
        };
        let tarred = Asset {
            label: "t",
            url: "",
            kind: AssetKind::TarBz2 { dir: "sub" },
            approx_bytes: 1,
        };
        assert!(!asset_is_present(&dir, &plain));
        assert!(!asset_is_present(&dir, &tarred));

        std::fs::write(dir.join("a.onnx"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(asset_is_present(&dir, &plain));
        assert!(asset_is_present(&dir, &tarred));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_matches_known_vector() {
        let dir = std::env::temp_dir().join(format!("vocmeet_sha_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc.txt");
        std::fs::write(&f, b"abc").unwrap();
        let (digest, size) = sha256_file(&f).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
