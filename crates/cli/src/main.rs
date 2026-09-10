//! VocMeet CLI：不依赖 GUI 即可跑通「转写 → 命名 → 纪要 → 导出」全链路。
//!
//! 存在的意义有两个：
//! 1. MVP 阶段的可验证入口——链路本身可以被脚本化地端到端测试；
//! 2. W1 bake-off 的评测载体（`transcribe --json` 输出可直接喂给评测脚本）。

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vocmeet_core::asr::{
    CancelToken, SherpaEngine, Stage, TranscribeJob, TranscriptionEngine,
};
use vocmeet_core::audio;
use vocmeet_core::config::Config;
use vocmeet_core::jobs::overall_progress;
use vocmeet_core::llm::LlmClient;
use vocmeet_core::models::ModelSet;
use vocmeet_core::store::{self, Store};
use vocmeet_core::summarize::{RenderInput, Summarizer, Template};
use vocmeet_core::{format_ts, Source, Utterance};

#[derive(Parser)]
#[command(name = "vocmeet-cli", about = "VocMeet 离线会议纪要 —— 命令行入口", version)]
struct Cli {
    /// 配置文件路径（不存在则用默认值）。
    #[arg(long, global = true, default_value = "vocmeet.config.json")]
    config: PathBuf,

    /// 覆盖模型目录。
    #[arg(long, global = true)]
    models: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 自检：模型是否齐全、校验和、DB 与 Keychain 是否可用。
    Doctor,

    /// 转写音频。至少提供 --system 或 --mic 之一。
    Transcribe {
        /// 系统回环轨 WAV（远端参会者，会做说话人分离）。
        #[arg(long)]
        system: Option<PathBuf>,
        /// 麦克风轨 WAV（本机用户，不做分离）。
        #[arg(long)]
        mic: Option<PathBuf>,
        /// 已知参会人数。省略则由聚类阈值自动估计。
        #[arg(long)]
        speakers: Option<i32>,
        /// 结果写入这场会议（省略则新建）。
        #[arg(long)]
        meeting: Option<i64>,
        /// 会议标题（新建时使用）。
        #[arg(long, default_value = "未命名会议")]
        title: String,
        /// 额外输出 JSON，供评测脚本使用。
        #[arg(long)]
        json: Option<PathBuf>,
    },

    /// 列出会议。
    List,

    /// 打印逐字稿。
    Show {
        meeting: i64,
        /// 只显示低置信片段（校对时用）。
        #[arg(long)]
        low_confidence_only: bool,
    },

    /// 给说话人命名，并回填到逐字稿与后续纪要。
    Name {
        meeting: i64,
        /// 形如 spk_0=张三
        #[arg(value_parser = parse_kv)]
        mapping: Vec<(String, String)>,
    },

    /// 写入速记笔记（Scratchpad）。
    Note { meeting: i64, text: String },

    /// 生成纪要。
    Summarize {
        meeting: i64,
        #[arg(long, default_value = "templates/meeting_default.yaml")]
        template: PathBuf,
        /// 关闭说话人姓名脱敏（默认开启，见 §5）。
        #[arg(long)]
        no_redact: bool,
    },

    /// 导出 Markdown（纪要 + 逐字稿）。
    Export {
        meeting: i64,
        #[arg(long)]
        out: PathBuf,
    },

    /// 查看出网审计记录条数。
    Audit,

    /// 列出音频设备，并探测系统回环是否可用。
    Devices,

    /// 录制双轨音频到分片文件（Ctrl+C 或 --seconds 到期后停止）。
    Record {
        /// 录制时长（秒）。省略则一直录到 Ctrl+C。
        #[arg(long)]
        seconds: Option<u64>,
        /// 分片输出目录。
        #[arg(long, default_value = "recordings")]
        out: PathBuf,
        /// 只录麦克风。
        #[arg(long)]
        mic_only: bool,
        /// 只录系统回环。
        #[arg(long)]
        system_only: bool,
    },
}

fn parse_kv(s: &str) -> std::result::Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("期望格式 spk_0=张三，得到 `{s}`"))?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    if let Some(m) = &cli.models {
        config.models_dir = m.clone();
    }
    config.ensure_dirs()?;

    match cli.command {
        Command::Doctor => doctor(&config),
        Command::Transcribe {
            system,
            mic,
            speakers,
            meeting,
            title,
            json,
        } => transcribe(&config, system, mic, speakers, meeting, &title, json),
        Command::List => list(&config),
        Command::Show {
            meeting,
            low_confidence_only,
        } => show(&config, meeting, low_confidence_only),
        Command::Name { meeting, mapping } => name(&config, meeting, &mapping),
        Command::Note { meeting, text } => note(&config, meeting, &text),
        Command::Summarize {
            meeting,
            template,
            no_redact,
        } => summarize(&config, meeting, &template, !no_redact),
        Command::Export { meeting, out } => export(&config, meeting, &out),
        Command::Audit => audit(&config),
        Command::Devices => devices(),
        Command::Record {
            seconds,
            out,
            mic_only,
            system_only,
        } => record(&config, seconds, out, mic_only, system_only),
    }
}

fn devices() -> Result<()> {
    println!("音频设备：");
    match vocmeet_capture::list_devices() {
        Ok(list) => {
            for d in list {
                let mark = if d.is_default { " (默认)" } else { "" };
                println!("  [{}] {}{}", d.direction, d.name, mark);
            }
        }
        Err(e) => println!("  枚举失败：{e}"),
    }
    println!();
    print!("系统回环探测：");
    match vocmeet_capture::probe_loopback() {
        Ok(()) => println!("可用"),
        Err(e) => println!("不可用 —— {e}"),
    }
    Ok(())
}

fn record(
    config: &Config,
    seconds: Option<u64>,
    out: PathBuf,
    mic_only: bool,
    system_only: bool,
) -> Result<()> {
    let cfg = vocmeet_capture::CaptureConfig {
        out_dir: out,
        chunk_seconds: config.capture.chunk_seconds,
        min_free_bytes: config.capture.min_free_bytes,
        record_mic: !system_only,
        record_system: !mic_only,
    };

    let stop = vocmeet_capture::StopSignal::new();
    if let Some(secs) = seconds {
        let s = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            s.stop();
        });
    }

    println!(
        "开始录制（麦克风 {}，系统回环 {}），输出到 {}",
        if cfg.record_mic { "开" } else { "关" },
        if cfg.record_system { "开" } else { "关" },
        cfg.out_dir.display()
    );
    if seconds.is_none() {
        println!("提示：未指定 --seconds，将持续录制直到进程被终止。");
    }

    let rec = vocmeet_capture::record_dual_track(&cfg, &stop)?;

    println!();
    println!("麦克风轨 {} 个分片", rec.mic_chunks.len());
    println!("系统回环轨 {} 个分片", rec.system_chunks.len());
    for w in &rec.warnings {
        println!("警告：{w}");
    }
    if !rec.mic_chunks.is_empty() || !rec.system_chunks.is_empty() {
        println!("
接着可以跑：");
        println!("  vocmeet-cli transcribe --system <sys 分片合并后的 wav> --mic <mic 分片合并后的 wav>");
    }
    Ok(())
}

fn open_store(config: &Config) -> Result<Store> {
    let key = store::master_key().context("读取/创建数据库主密钥失败（Keychain 不可用？）")?;
    Store::open(config.db_path(), &key).context("打开数据库失败")
}

fn doctor(config: &Config) -> Result<()> {
    println!("VocMeet 自检");
    println!("  数据目录 : {}", config.data_dir.display());
    println!("  模型目录 : {}", config.models_dir.display());
    println!("  出网策略 : {:?}", config.egress_policy);
    println!("  LLM 端点 : {}", config.llm.api_base);
    println!();

    let models = ModelSet::from_root(&config.models_dir);
    match models.verify_present() {
        Ok(()) => {
            println!("模型完整性：");
            for (name, digest, size) in models.checksums()? {
                println!(
                    "  {name:20} {:>8.1} MB  sha256:{}",
                    size as f64 / 1e6,
                    &digest[..16]
                );
            }
            println!("  合计 {:.1} MB", models.total_size()? as f64 / 1e6);
        }
        Err(e) => {
            println!("模型检查失败：{e}");
            println!(
                "请先运行：bash scripts/fetch-models.sh {}",
                config.models_dir.display()
            );
            return Ok(());
        }
    }

    println!();
    match open_store(config) {
        Ok(s) => println!(
            "数据库与 Keychain：正常（{} 场会议）",
            s.list_meetings()?.len()
        ),
        Err(e) => println!("数据库与 Keychain：失败 —— {e:#}"),
    }
    Ok(())
}

fn load_track(path: Option<PathBuf>) -> Result<Option<audio::Pcm>> {
    let Some(p) = path else { return Ok(None) };
    let pcm = audio::read_wav(&p).with_context(|| format!("读取 {}", p.display()))?;
    Ok(Some(audio::resample_to_target(&pcm)?))
}

fn transcribe(
    config: &Config,
    system: Option<PathBuf>,
    mic: Option<PathBuf>,
    speakers: Option<i32>,
    meeting: Option<i64>,
    title: &str,
    json_out: Option<PathBuf>,
) -> Result<()> {
    anyhow::ensure!(
        system.is_some() || mic.is_some(),
        "至少提供 --system 或 --mic 之一"
    );

    let models = ModelSet::from_root(&config.models_dir);
    let t0 = std::time::Instant::now();
    eprintln!("加载模型…");
    let engine = SherpaEngine::new(&models, config.engine.to_options())?;
    eprintln!("模型加载完成，用时 {:.1}s", t0.elapsed().as_secs_f64());

    let job = TranscribeJob {
        system: load_track(system)?,
        mic: load_track(mic)?,
        known_speakers: speakers,
    };

    let secs = |p: &Option<audio::Pcm>| {
        p.as_ref()
            .map(|x| x.samples.len() as f64 / 16_000.0)
            .unwrap_or(0.0)
    };
    let audio_secs = secs(&job.system) + secs(&job.mic);

    let cancel = CancelToken::new();
    let last_pct = std::cell::Cell::new(-1i32);
    let progress = |p: vocmeet_core::asr::Progress| {
        let pct = (overall_progress(p.stage, p.fraction) * 100.0) as i32;
        if pct != last_pct.get() {
            eprint!("\r  [{pct:3}%] {:?} {}            ", p.stage, p.detail);
            last_pct.set(pct);
        }
        if p.stage == Stage::Done {
            eprintln!();
        }
    };

    let t1 = std::time::Instant::now();
    let utterances = engine.transcribe(&job, &progress, &cancel)?;
    let elapsed = t1.elapsed().as_secs_f64();

    println!();
    println!("转写完成：{} 条发言", utterances.len());
    if audio_secs > 0.0 {
        println!(
            "音频 {:.1}s，耗时 {:.1}s，RTF = {:.3}",
            audio_secs,
            elapsed,
            elapsed / audio_secs
        );
    }
    let low = utterances.iter().filter(|u| u.low_confidence).count();
    let pct = if utterances.is_empty() {
        0.0
    } else {
        low as f64 * 100.0 / utterances.len() as f64
    };
    println!("低置信片段 {low} 条（{pct:.0}%），需人工校对");

    let found: std::collections::BTreeSet<&str> =
        utterances.iter().map(|u| u.speaker_id.as_str()).collect();
    println!(
        "识别到说话人：{}",
        found.into_iter().collect::<Vec<_>>().join(", ")
    );

    let s = open_store(config)?;
    let meeting_id = match meeting {
        Some(id) => id,
        None => s.create_meeting(title, &now_iso())?,
    };
    s.replace_utterances(meeting_id, &utterances)?;
    s.set_meeting_status(meeting_id, "transcribed")?;
    println!("已写入会议 #{meeting_id}");

    if let Some(path) = json_out {
        std::fs::write(&path, serde_json::to_string_pretty(&utterances)?)?;
        println!("JSON 已写入 {}", path.display());
    }

    println!();
    print_transcript(&utterances, false, 12);
    Ok(())
}

fn print_transcript(utterances: &[Utterance], low_only: bool, limit: usize) {
    let shown: Vec<&Utterance> = utterances
        .iter()
        .filter(|u| !low_only || u.low_confidence)
        .collect();
    let total = shown.len();
    for u in shown.iter().take(limit) {
        let mark = if u.low_confidence { "  (?)" } else { "" };
        let track = match u.source {
            Source::Mic => "麦",
            Source::System => "系",
        };
        println!(
            "[#{:>3}] {track} {:<10} {}  {}{mark}",
            u.id,
            u.display_speaker(),
            format_ts(u.start_ms),
            u.text
        );
    }
    if total > limit {
        println!("… 其余 {} 条省略（用 show 查看全部）", total - limit);
    }
}

fn list(config: &Config) -> Result<()> {
    let s = open_store(config)?;
    let meetings = s.list_meetings()?;
    if meetings.is_empty() {
        println!("暂无会议。先跑 transcribe。");
        return Ok(());
    }
    println!("{:<5} {:<26} {:<13} {:<22}", "ID", "标题", "状态", "开始时间");
    for m in meetings {
        println!(
            "{:<5} {:<26} {:<13} {:<22}",
            m.id, m.title, m.status, m.started_at
        );
    }
    Ok(())
}

fn show(config: &Config, meeting: i64, low_only: bool) -> Result<()> {
    let s = open_store(config)?;
    let utterances = s.load_utterances(meeting)?;
    if utterances.is_empty() {
        println!("会议 #{meeting} 没有逐字稿。");
        return Ok(());
    }
    print_transcript(&utterances, low_only, usize::MAX);
    Ok(())
}

fn name(config: &Config, meeting: i64, mapping: &[(String, String)]) -> Result<()> {
    let s = open_store(config)?;
    if mapping.is_empty() {
        let utterances = s.load_utterances(meeting)?;
        println!("这场会议里出现的说话人：");
        for key in s.distinct_speakers(meeting)? {
            let sample = utterances
                .iter()
                .find(|u| u.speaker_id == key)
                .map(|u| u.text.chars().take(30).collect::<String>())
                .unwrap_or_default();
            println!("  {key:<12} 例：{sample}");
        }
        println!("\n用法： vocmeet-cli name {meeting} spk_0=张三 spk_1=李四");
        return Ok(());
    }
    for (k, v) in mapping {
        s.name_speaker(meeting, k, v)?;
        println!("  {k} -> {v}");
    }
    println!("已回填到逐字稿与后续纪要。");
    Ok(())
}

fn note(config: &Config, meeting: i64, text: &str) -> Result<()> {
    let s = open_store(config)?;
    s.save_note(meeting, text)?;
    println!("速记已保存（{} 字）。", text.chars().count());
    Ok(())
}

fn summarize(config: &Config, meeting: i64, template_path: &PathBuf, redact: bool) -> Result<()> {
    let s = open_store(config)?;
    let utterances = s.load_utterances(meeting)?;
    anyhow::ensure!(!utterances.is_empty(), "会议 #{meeting} 没有逐字稿");

    let m = s.get_meeting(meeting)?.context("会议不存在")?;
    let scratchpad = s.load_note(meeting)?;
    let template = Template::load(template_path)?;

    let attendees: Vec<String> = {
        let mut v: Vec<String> = utterances
            .iter()
            .map(|u| u.display_speaker().to_string())
            .collect();
        v.sort();
        v.dedup();
        v
    };

    let client = LlmClient::new(config.llm.clone(), config.egress_policy, store::get_api_key())?;

    let input = RenderInput {
        meeting_title: &m.title,
        meeting_time: &m.started_at,
        attendees: &attendees,
        scratchpad: &scratchpad,
        utterances: &utterances,
    };

    let summarizer = Summarizer {
        client: &client,
        template: &template,
        config: &config.llm,
        redact_names: redact,
    };

    eprintln!(
        "生成纪要中（模型 {}，脱敏 {}）…\n",
        config.llm.model,
        if redact { "开" } else { "关" }
    );

    let rt = tokio::runtime::Runtime::new()?;
    let out = rt.block_on(async {
        let mut on_delta = |d: &str| {
            use std::io::Write;
            print!("{d}");
            let _ = std::io::stdout().flush();
        };
        summarizer.run(&input, &mut on_delta).await
    })?;

    println!("\n");
    for rec in &out.egress {
        s.log_egress(Some(meeting), rec)?;
    }
    let id = s.save_summary(meeting, &config.llm.model, &out.markdown)?;
    println!(
        "纪要 #{id} 已保存（{}，{} 次出网已记入审计）",
        if out.used_map_reduce {
            "map-reduce"
        } else {
            "单次生成"
        },
        out.egress.len()
    );
    Ok(())
}

fn export(config: &Config, meeting: i64, out: &PathBuf) -> Result<()> {
    let s = open_store(config)?;
    let m = s.get_meeting(meeting)?.context("会议不存在")?;
    let utterances = s.load_utterances(meeting)?;
    let summary = s.latest_summary(meeting)?;

    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", m.title));
    md.push_str(&format!("- 开始时间：{}\n", m.started_at));
    md.push_str(&format!("- 发言条数：{}\n\n", utterances.len()));

    if let Some(sum) = summary {
        md.push_str("## 会议纪要\n\n");
        md.push_str(&sum);
        md.push_str("\n\n");
    }

    md.push_str("## 逐字稿\n\n");
    for u in &utterances {
        let mark = if u.low_confidence {
            " *(识别存疑)*"
        } else {
            ""
        };
        md.push_str(&format!(
            "**[#{}] {} `{}`**：{}{}\n\n",
            u.id,
            u.display_speaker(),
            format_ts(u.start_ms),
            u.text,
            mark
        ));
    }

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, md)?;
    println!("已导出到 {}", out.display());
    Ok(())
}

fn audit(config: &Config) -> Result<()> {
    let s = open_store(config)?;
    println!("出网记录 {} 条（只含元数据，不含内容）", s.egress_count()?);
    Ok(())
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days 算法：避免为一个时间戳引入完整时区依赖。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
