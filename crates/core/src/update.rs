//! 公开、无密钥的更新检查。
//!
//! 不用 Tauri 官方 updater 插件——那条路要求维护一对签名密钥，仓库私有时还好，
//! 仓库转为 public 后就没必要再背这份密钥管理成本。这里直接读 GitHub Releases
//! 的公开 API 拿最新 tag 和附件链接，下载回来的是官方 Release 产物本身，
//! 交给系统自带的安装器（NSIS/MSI/DMG）去装——信任边界等同于用户自己去
//! Releases 页面手动下载安装，不做签名校验。

use crate::{Error, Result};
use serde::Deserialize;
use std::path::Path;

/// 挑安装包时要匹配的目标系统。做成参数而不是直接 `cfg!` 判断，
/// 是为了让 `pick_asset` 能在任意平台的 CI 上把三套分支都测到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Windows,
    MacOs,
    Linux,
}

impl TargetOs {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            TargetOs::Windows
        } else if cfg!(target_os = "macos") {
            TargetOs::MacOs
        } else {
            TargetOs::Linux
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateCheck {
    /// 有更新，且找到了匹配当前系统/架构的安装包——可以直接给用户一个「安装更新」按钮。
    pub available: bool,
    /// 单纯的版本号比较结果：只要求 latest > current，不管有没有对应安装包。
    /// `available == false` 但这个是 `true`，说明发了新版本但这个平台/架构没有对应产物。
    pub newer_version_exists: bool,
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

#[derive(Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// 拿 `owner/repo` 的最新 Release，和 `current_version` 比较。
pub async fn check_latest(repo: &str, current_version: &str) -> Result<UpdateCheck> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent(format!("vocmeet-updater/{current_version}"))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
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
    let newer_version_exists = is_newer(&latest_version, current_version);
    let asset = pick_asset(&release.assets, TargetOs::current(), std::env::consts::ARCH);

    Ok(UpdateCheck {
        available: newer_version_exists && asset.is_some(),
        newer_version_exists,
        latest_version,
        notes: release.body,
        asset_url: asset.map(|a| a.browser_download_url.clone()),
        asset_name: asset.map(|a| a.name.clone()),
    })
}

fn matches_os(lower_name: &str, os: TargetOs) -> bool {
    match os {
        TargetOs::Windows => lower_name.ends_with(".exe") || lower_name.ends_with(".msi"),
        TargetOs::MacOs => lower_name.ends_with(".dmg"),
        TargetOs::Linux => {
            lower_name.ends_with(".appimage")
                || lower_name.ends_with(".deb")
                || lower_name.ends_with(".rpm")
        }
    }
}

/// 从文件名里猜架构。判断顺序有讲究："x86_64" 本身就包含 "x86" 子串，
/// 必须先判更具体的 x86_64/aarch64，剩下的才轮到裸 "x86"，否则 64 位包会被
/// 误判成 32 位。
fn detect_asset_arch(lower_name: &str) -> Option<&'static str> {
    if lower_name.contains("aarch64") || lower_name.contains("arm64") {
        Some("aarch64")
    } else if lower_name.contains("x86_64") || lower_name.contains("amd64") || lower_name.contains("x64")
    {
        Some("x86_64")
    } else if lower_name.contains("x86")
        || lower_name.contains("i686")
        || lower_name.contains("ia32")
        || lower_name.contains("i386")
    {
        Some("x86")
    } else {
        None
    }
}

/// 按当前系统 + 架构挑一个能直接装的附件。
///
/// 架构不匹配的直接排除（避免把 arm64 包装到 x64 机器上），架构不明的
/// （没在文件名里标出来的通用包）当兜底保留；同架构里 Windows 下 .exe 优先于 .msi，
/// Linux 下 .AppImage 优先于 .deb/.rpm，免交互，双击即走。
fn pick_asset<'a>(assets: &'a [GhAsset], os: TargetOs, arch: &str) -> Option<&'a GhAsset> {
    let named: Vec<(String, &GhAsset)> = assets
        .iter()
        .map(|a| (a.name.to_ascii_lowercase(), a))
        .filter(|(name, _)| matches_os(name, os))
        .filter(|(name, _)| match detect_asset_arch(name) {
            Some(detected) => detected == arch,
            None => true,
        })
        .collect();

    named
        .into_iter()
        .min_by_key(|(name, _)| {
            let arch_rank: u8 = u8::from(detect_asset_arch(name).is_none());
            let ext_rank: u8 = ext_rank(name, os);
            (arch_rank, ext_rank)
        })
        .map(|(_, a)| a)
}

fn ext_rank(lower_name: &str, os: TargetOs) -> u8 {
    match os {
        TargetOs::Windows => u8::from(lower_name.ends_with(".msi")),
        TargetOs::MacOs => 0,
        TargetOs::Linux => {
            if lower_name.ends_with(".appimage") {
                0
            } else if lower_name.ends_with(".deb") {
                1
            } else {
                2
            }
        }
    }
}

/// 版本号比较优先走标准 semver（正确处理 `1.0.0-rc.1 < 1.0.0` 这种预发布语义）。
/// 万一 tag 不是严格 semver（缺 patch 号等），退化成按 `.`/`-`/`+` 拆开逐段比数字，
/// 比直接不让升级更实用。
fn is_newer(latest: &str, current: &str) -> bool {
    match (semver::Version::parse(latest), semver::Version::parse(current)) {
        (Ok(l), Ok(c)) => l > c,
        _ => is_newer_fallback(latest, current),
    }
}

fn is_newer_fallback(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(|c: char| c == '.' || c == '-' || c == '+')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let l = parts(latest);
    let c = parts(current);
    for i in 0..l.len().max(c.len()) {
        let lv = l.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if lv != cv {
            return lv > cv;
        }
    }
    false
}

/// 流式下载到 `dst`：先落到同目录下的 `<file>.part`，成功了再 rename 成品——
/// 中途失败或被取消都不会在磁盘上留下一个看着像完整安装包的半成品文件。
/// 每攒够 1MB 回报一次进度；`cancel` 每个 chunk 探测一次。
pub async fn download_update(
    url: &str,
    dst: &Path,
    on_progress: impl Fn(u64, Option<u64>),
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    use futures_util::StreamExt;
    use std::io::Write;

    let client = reqwest::Client::builder()
        .user_agent("vocmeet-updater")
        .connect_timeout(std::time::Duration::from_secs(10))
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

    let file_name = dst
        .file_name()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("非法的下载目标路径")))?;
    let part = dst.with_file_name(format!("{}.part", file_name.to_string_lossy()));

    let outcome = (|| async {
        let mut file = std::fs::File::create(&part).map_err(|e| Error::io(&part, e))?;
        let mut stream = resp.bytes_stream();
        let mut got = 0u64;
        let mut last_report = 0u64;
        while let Some(chunk) = stream.next().await {
            if cancel() {
                return Err(Error::Cancelled);
            }
            let chunk = chunk.map_err(|e| Error::Other(anyhow::anyhow!("下载中断: {e}")))?;
            file.write_all(&chunk).map_err(|e| Error::io(&part, e))?;
            got += chunk.len() as u64;
            if got - last_report >= 1 << 20 {
                last_report = got;
                on_progress(got, total);
            }
        }
        file.flush().map_err(|e| Error::io(&part, e))?;
        on_progress(got, total);
        Ok(())
    })()
    .await;

    if let Err(e) = outcome {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    std::fs::rename(&part, dst).map_err(|e| Error::io(dst, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GhAsset {
        GhAsset {
            name: name.to_string(),
            browser_download_url: format!("http://example.invalid/{name}"),
        }
    }

    #[test]
    fn version_compare() {
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(is_newer("0.4.0", "0.3.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        // 预发布版本语义上比正式版"更旧"，不该被当成可升级的新版本推送。
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
    }

    #[test]
    fn version_compare_fallback_for_non_semver_tags() {
        // 缺 patch 号这种非严格 semver 也不该直接放弃比较。
        assert!(is_newer_fallback("0.4", "0.3.9"));
        assert!(!is_newer_fallback("0.3", "0.3.0"));
    }

    #[test]
    fn asset_pick_prefers_exe_over_msi_on_windows() {
        let assets = vec![
            asset("VocMeet_0.3.1_x64_en-US.msi"),
            asset("VocMeet_0.3.1_x64-setup.exe"),
        ];
        let picked = pick_asset(&assets, TargetOs::Windows, "x86_64").unwrap();
        assert!(picked.name.ends_with(".exe"));
    }

    #[test]
    fn asset_pick_matches_arch_on_windows() {
        let assets = vec![
            asset("VocMeet_0.3.1_arm64-setup.exe"),
            asset("VocMeet_0.3.1_x64-setup.exe"),
        ];
        let picked = pick_asset(&assets, TargetOs::Windows, "x86_64").unwrap();
        assert_eq!(picked.name, "VocMeet_0.3.1_x64-setup.exe");

        let picked = pick_asset(&assets, TargetOs::Windows, "aarch64").unwrap();
        assert_eq!(picked.name, "VocMeet_0.3.1_arm64-setup.exe");
    }

    #[test]
    fn asset_pick_dmg_on_macos_matches_arch() {
        let assets = vec![
            asset("VocMeet_0.3.1_aarch64.dmg"),
            asset("VocMeet_0.3.1_x64.dmg"),
        ];
        let picked = pick_asset(&assets, TargetOs::MacOs, "aarch64").unwrap();
        assert_eq!(picked.name, "VocMeet_0.3.1_aarch64.dmg");
    }

    #[test]
    fn asset_pick_linux_prefers_appimage() {
        let assets = vec![
            asset("VocMeet_0.3.1_amd64.deb"),
            asset("VocMeet_0.3.1_amd64.AppImage"),
        ];
        let picked = pick_asset(&assets, TargetOs::Linux, "x86_64").unwrap();
        assert!(picked.name.to_ascii_lowercase().ends_with(".appimage"));
    }

    #[test]
    fn asset_pick_linux_prefers_deb_over_rpm() {
        let assets = vec![
            asset("VocMeet_0.3.1_amd64.rpm"),
            asset("VocMeet_0.3.1_amd64.deb"),
        ];
        let picked = pick_asset(&assets, TargetOs::Linux, "x86_64").unwrap();
        assert!(picked.name.to_ascii_lowercase().ends_with(".deb"));
    }

    #[test]
    fn asset_pick_none_when_platform_missing() {
        let assets = vec![asset("VocMeet_0.3.1_amd64.deb")];
        assert!(pick_asset(&assets, TargetOs::Windows, "x86_64").is_none());
    }

    #[test]
    fn asset_pick_none_when_only_wrong_arch_available() {
        let assets = vec![asset("VocMeet_0.3.1_arm64-setup.exe")];
        assert!(pick_asset(&assets, TargetOs::Windows, "x86_64").is_none());
    }
}
