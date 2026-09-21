//! 公开、无密钥的更新检查。
//!
//! 不用 Tauri 官方 updater 插件——那条路要求维护一对签名密钥，仓库私有时还好，
//! 仓库转为 public 后就没必要再背这份密钥管理成本。这里直接读 GitHub Releases
//! 的公开 API 拿最新 tag 和附件链接，下载回来的是官方 Release 产物本身，
//! 交给系统自带的安装器（NSIS/MSI/DMG）去装——信任边界等同于用户自己去
//! Releases 页面手动下载安装，不做签名校验。

use crate::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct UpdateCheck {
    pub available: bool,
    pub latest_version: String,
    pub notes: String,
    pub asset_url: Option<String>,
    pub asset_name: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// 拿 `owner/repo` 的最新 Release，和 `current_version` 比较。
pub async fn check_latest(repo: &str, current_version: &str) -> Result<UpdateCheck> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent(format!("vocmeet-updater/{current_version}"))
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("构建 HTTP 客户端失败: {e}")))?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("检查更新失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(anyhow::anyhow!(
            "GitHub 返回 HTTP {}",
            resp.status()
        )));
    }
    let release: GhRelease = resp
        .json()
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("解析 Release 信息失败: {e}")))?;

    let latest_version = release.tag_name.trim_start_matches('v').to_string();
    let available = is_newer(&latest_version, current_version);
    let asset = pick_asset(&release.assets);

    Ok(UpdateCheck {
        available,
        latest_version,
        notes: release.body,
        asset_url: asset.map(|a| a.browser_download_url.clone()),
        asset_name: asset.map(|a| a.name.clone()),
    })
}

/// 按当前平台挑一个能直接双击安装的附件：Windows 优先 .exe（免交互），
/// 其次 .msi；macOS 挑 .dmg；Linux 挑 AppImage/deb/rpm。
fn pick_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    fn matches(name: &str) -> bool {
        if cfg!(target_os = "windows") {
            name.ends_with(".exe") || name.ends_with(".msi")
        } else if cfg!(target_os = "macos") {
            name.ends_with(".dmg")
        } else {
            name.ends_with(".appimage") || name.ends_with(".deb") || name.ends_with(".rpm")
        }
    }
    assets
        .iter()
        .filter(|a| matches(&a.name.to_ascii_lowercase()))
        .min_by_key(|a| u8::from(a.name.to_ascii_lowercase().ends_with(".msi")))
}

fn parse_version(v: &str) -> Vec<u64> {
    v.split(|c: char| c == '.' || c == '-' || c == '+')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let l = parse_version(latest);
    let c = parse_version(current);
    for i in 0..l.len().max(c.len()) {
        let lv = l.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if lv != cv {
            return lv > cv;
        }
    }
    false
}

/// 流式下载到 `dst`，每攒够 1MB 回报一次进度。
pub async fn download_update(
    url: &str,
    dst: &std::path::Path,
    on_progress: impl Fn(u64, Option<u64>),
) -> Result<()> {
    use futures_util::StreamExt;
    use std::io::Write;

    let client = reqwest::Client::builder()
        .user_agent("vocmeet-updater")
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("构建 HTTP 客户端失败: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("下载更新失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(anyhow::anyhow!(
            "下载更新失败：HTTP {}",
            resp.status()
        )));
    }
    let total = resp.content_length();
    let mut file = std::fs::File::create(dst).map_err(|e| Error::io(dst, e))?;
    let mut stream = resp.bytes_stream();
    let mut got = 0u64;
    let mut last_report = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Other(anyhow::anyhow!("下载中断: {e}")))?;
        file.write_all(&chunk).map_err(|e| Error::io(dst, e))?;
        got += chunk.len() as u64;
        if got - last_report >= 1 << 20 {
            last_report = got;
            on_progress(got, total);
        }
    }
    file.flush().map_err(|e| Error::io(dst, e))?;
    on_progress(got, total);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(is_newer("0.4.0", "0.3.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
    }

    #[test]
    fn asset_pick_prefers_exe_over_msi_on_windows() {
        if !cfg!(target_os = "windows") {
            return;
        }
        let assets = vec![
            GhAsset {
                name: "VocMeet_0.3.1_x64_en-US.msi".into(),
                browser_download_url: "http://x/msi".into(),
            },
            GhAsset {
                name: "VocMeet_0.3.1_x64-setup.exe".into(),
                browser_download_url: "http://x/exe".into(),
            },
        ];
        let picked = pick_asset(&assets).unwrap();
        assert!(picked.name.ends_with(".exe"));
    }
}
