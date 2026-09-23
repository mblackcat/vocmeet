#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! VocMeet 桌面应用。UI 通过 Tauri command 调用 core，长任务用 event 回推进度。
//!
//! 设计约束：
//! - 每个命令自己开 Store（SQLite 打开很便宜，WAL 支持并发），避免把
//!   非 Sync 的 `Connection` 塞进全局 state；
//! - 转写与纪要生成在独立线程/运行时里跑，UI 线程永不阻塞；
//! - 录制状态（StopSignal）是唯一需要跨命令共享的可变状态。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
#[cfg(windows)]
use tauri::WindowEvent;

use vocmeet_core::asr::{
    CancelToken, Progress, SherpaEngine, Stage, TranscribeJob, TranscriptionEngine,
};
use vocmeet_core::audio::{self, ChunkInfo};
use vocmeet_core::config::Config;
use vocmeet_core::jobs::overall_progress;
use vocmeet_core::llm::LlmClient;
use vocmeet_core::models::ModelSet;
use vocmeet_core::policy::EgressPolicy;
use vocmeet_core::store::{self, Meeting, Store};
use vocmeet_core::summarize::{RenderInput, Summarizer, Template};
use vocmeet_core::{format_ts, Utterance, TARGET_SAMPLE_RATE};

// ---------------------------------------------------------------- 状态

struct RecordingSession {
    meeting_id: i64,
    stop: vocmeet_capture::StopSignal,
    handle: std::thread::JoinHandle<vocmeet_capture::Result<vocmeet_capture::Recording>>,
    /// 静音看门狗。必须与采集线程分开停——它靠挂钟判断，
    /// 而「一个回调都没来」正是它要抓的情况，不能由数据驱动。
    watchdog_stop: Arc<AtomicBool>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    /// 会中实时转写线程。只服务于「会中看得见」——
    /// 会后一律整轨重算，所以这里不保留它的产出。
    live: Option<std::thread::JoinHandle<()>>,
    live_dropped: Arc<AtomicU64>,
}

struct AppState {
    config: Mutex<Config>,
    config_path: PathBuf,
    master_key: Vec<u8>,
    recording: Mutex<Option<RecordingSession>>,
    /// 正在进行的转写任务的取消令牌。
    transcribe_cancel: Mutex<Option<CancelToken>>,
    /// 上一次录制产出的分片，供转写直接取用。
    last_chunks: Arc<Mutex<Option<(Vec<ChunkInfo>, Vec<ChunkInfo>)>>>,
    /// 正在进行的权重下载的取消令牌。与转写各管各的：
    /// 下模型和转写没有资源冲突，没道理互相挡着。
    model_fetch_cancel: Mutex<Option<CancelToken>>,
    /// 是否正在让 Ollama 拉模型。拉取由 Ollama 自己完成，我们只能防重入。
    llm_pulling: Mutex<bool>,
    /// 最近一次 `check_update` 查到的可用更新，供 `install_update` 直接取用。
    /// 装哪个包由后端自己说了算，不接受前端传来的任意 URL/文件名。
    pending_update: Mutex<Option<vocmeet_core::update::UpdateCheck>>,
    /// 正在进行的更新下载的取消令牌，兼防重入。
    update_cancel: Mutex<Option<CancelToken>>,
}

impl AppState {
    fn store(&self) -> Result<Store, String> {
        let cfg = self.config.lock().map_err(|_| "配置锁中毒")?;
        Store::open(cfg.db_path(), &self.master_key).map_err(|e| e.to_string())
    }
    fn config(&self) -> Result<Config, String> {
        Ok(self.config.lock().map_err(|_| "配置锁中毒")?.clone())
    }
}

type R<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- DTO

#[derive(Serialize)]
struct DoctorReport {
    data_dir: String,
    models_dir: String,
    egress_policy: String,
    llm_endpoint: String,
    llm_model: String,
    models_ok: bool,
    models_message: String,
    model_files: Vec<ModelFile>,
    total_model_mb: f64,
    db_ok: bool,
    db_message: String,
    loopback_ok: bool,
    loopback_message: String,
}

#[derive(Serialize)]
struct ModelFile {
    name: String,
    size_mb: f64,
    sha256_prefix: String,
}

#[derive(Serialize)]
struct SpeakerRow {
    key: String,
    display_name: Option<String>,
    sample: String,
    utterance_count: usize,
}

#[derive(Serialize, Clone)]
struct ProgressEvent {
    meeting_id: i64,
    stage: String,
    progress: f32,
    detail: String,
}

#[derive(Serialize, Clone)]
struct DoneEvent {
    meeting_id: i64,
    ok: bool,
    message: String,
    utterance_count: usize,
}

#[derive(Serialize, Clone)]
struct DeltaEvent {
    meeting_id: i64,
    delta: String,
}

// ---------------------------------------------------------------- 命令：自检与设备

#[tauri::command]
fn doctor(state: State<AppState>) -> R<DoctorReport> {
    let cfg = state.config()?;
    let models = ModelSet::from_root(&cfg.models_dir);

    // 只看文件在不在、占多大。整包 SHA256 要读几百 MB，设置页每次打开都算一遍会卡住。
    let (models_ok, models_message, model_files, total_mb) = match models.verify_present() {
        Ok(()) => {
            let total = models.total_size().unwrap_or(0);
            let files = models
                .entries()
                .into_iter()
                .map(|(name, path)| ModelFile {
                    name: name.to_string(),
                    size_mb: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6,
                    sha256_prefix: String::new(),
                })
                .collect::<Vec<_>>();
            (true, "全部权重就位".to_string(), files, total as f64 / 1e6)
        }
        Err(e) => (
            false,
            // 设置页现在有一键下载，指路先指按钮；脚本留给无 GUI 的场景。
            format!(
                "{e}\n用下面的「一键下载模型」，或者命令行跑：bash scripts/fetch-models.sh {}",
                cfg.models_dir.display()
            ),
            Vec::new(),
            0.0,
        ),
    };

    let (db_ok, db_message) = match state.store() {
        Ok(s) => match s.list_meetings() {
            Ok(m) => (true, format!("正常（{} 场会议）", m.len())),
            Err(e) => (false, e.to_string()),
        },
        Err(e) => (false, e),
    };

    let (loopback_ok, loopback_message) = match vocmeet_capture::probe_loopback() {
        Ok(msg) => (true, msg),
        Err(e) => (false, e.to_string()),
    };

    Ok(DoctorReport {
        data_dir: cfg.data_dir.display().to_string(),
        models_dir: cfg.models_dir.display().to_string(),
        egress_policy: format!("{:?}", cfg.egress_policy),
        llm_endpoint: cfg.llm.api_base.clone(),
        llm_model: cfg.llm.model.clone(),
        models_ok,
        models_message,
        model_files,
        total_model_mb: total_mb,
        db_ok,
        db_message,
        loopback_ok,
        loopback_message,
    })
}

#[tauri::command]
fn list_devices() -> R<Vec<vocmeet_capture::DeviceInfo>> {
    vocmeet_capture::list_devices().map_err(err)
}

// ---------------------------------------------------------------- 命令：会议

#[tauri::command]
fn list_meetings(state: State<AppState>) -> R<Vec<Meeting>> {
    state.store()?.list_meetings().map_err(err)
}

#[derive(Serialize)]
struct MeetingPage {
    items: Vec<Meeting>,
    total: i64,
    offset: i64,
}

/// 侧栏分页。会议数量会随使用增长，不能一次性全加载。
#[tauri::command]
fn list_meetings_page(state: State<AppState>, offset: i64, limit: i64) -> R<MeetingPage> {
    let store = state.store()?;
    Ok(MeetingPage {
        items: store.list_meetings_page(offset, limit).map_err(err)?,
        total: store.count_meetings().map_err(err)?,
        offset,
    })
}

#[derive(Serialize)]
struct MeetingDetail {
    meeting: Meeting,
    keywords: Vec<String>,
    attendees: Vec<String>,
    summary: Option<String>,
    playback_path: Option<String>,
    transcript_path: Option<String>,
    utterance_count: usize,
    note: String,
}

/// 已完成会议的完整视图数据，一次取齐，避免前端串行多次 invoke。
#[tauri::command]
fn get_meeting_detail(state: State<AppState>, meeting_id: i64) -> R<MeetingDetail> {
    let store = state.store()?;
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or("会议不存在")?;
    let utterances = store.load_utterances(meeting_id).map_err(err)?;

    // 参会人直接来自逐字稿里出现过的说话人，不需要模型参与。
    let mut attendees: Vec<String> = utterances
        .iter()
        .map(|u| u.display_speaker().to_string())
        .collect();
    attendees.sort();
    attendees.dedup();

    Ok(MeetingDetail {
        keywords: store.get_keywords(meeting_id).map_err(err)?,
        summary: store.latest_summary(meeting_id).map_err(err)?,
        playback_path: store.get_playback_path(meeting_id).map_err(err)?,
        transcript_path: store.get_transcript_path(meeting_id).map_err(err)?,
        utterance_count: utterances.len(),
        note: store.load_note(meeting_id).map_err(err)?,
        attendees,
        meeting,
    })
}

#[tauri::command]
fn get_meeting(state: State<AppState>, meeting_id: i64) -> R<Option<Meeting>> {
    state.store()?.get_meeting(meeting_id).map_err(err)
}

#[tauri::command]
fn delete_meeting(state: State<AppState>, meeting_id: i64) -> R<()> {
    state.store()?.delete_meeting(meeting_id).map_err(err)
}

// ---------------------------------------------------------------- 命令：录制

/// 推给前端的实时电平。每轨约 20 Hz。
#[derive(Serialize, Clone)]
struct LevelEvent {
    /// "Mic" 或 "System"，与 `vocmeet_core::Source` 的 Debug 名对齐。
    source: String,
    rms: f32,
    peak: f32,
}

/// 录制过程中的告警。目前只有系统轨静音一种。
#[derive(Serialize, Clone)]
struct RecordingWarningEvent {
    meeting_id: i64,
    kind: String,
    message: String,
}

/// 系统轨静默多久就告警。够长以躲开会议开场的安静，又远短于一场会。
const SILENCE_ALERT_AFTER: Duration = Duration::from_secs(10);

/// 实时转写待处理队列的上限（段数）。
///
/// 超了就丢最旧的段。15 秒一段、上限 4 段 = 允许转写落后约一分钟；
/// 再落后说明这台机器跟不上，继续排队只会越积越多且吃内存，
/// 不如丢掉交给会后那次完整转写——那条路径本来就兜得住。
const LIVE_QUEUE_LIMIT: u64 = 4;

/// 实时逐字稿的一行。
#[derive(Serialize, Clone)]
struct LiveLine {
    start_ms: u32,
    /// "Mic" 或 "System"。会中拿不到真实说话人身份，前端据此显示「我 / 对方」。
    source: String,
    text: String,
}

#[derive(Serialize, Clone)]
struct LiveTranscriptEvent {
    meeting_id: i64,
    lines: Vec<LiveLine>,
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<AppState>, title: String) -> R<i64> {
    {
        let guard = state.recording.lock().map_err(|_| "录制锁中毒")?;
        if guard.is_some() {
            return Err("已有录制在进行中".into());
        }
    }

    let cfg = state.config()?;
    let store = state.store()?;
    let meeting_id = store
        .create_meeting(&title, &now_iso())
        .map_err(err)?;

    let out_dir = cfg.audio_dir().join(format!("meeting_{meeting_id}"));
    let record_system = cfg.capture.record_system;
    let capture_cfg = vocmeet_capture::CaptureConfig {
        out_dir,
        chunk_seconds: cfg.capture.chunk_seconds,
        min_free_bytes: cfg.capture.min_free_bytes,
        record_mic: cfg.capture.record_mic,
        record_system,
        live_segment_seconds: cfg.capture.live_segment_seconds,
    };

    // 系统轨最后一次听到声音的时刻，以录制开始为 0 点，单位毫秒。
    // 看门狗线程读它，采集线程写它——用原子量而不是锁，免得拖慢采集回调。
    let started = Instant::now();
    let last_audio_ms = Arc::new(AtomicU64::new(0));

    let level_app = app.clone();
    let level_seen = last_audio_ms.clone();
    let sink: vocmeet_capture::LevelSink = Arc::new(move |s: vocmeet_capture::LevelSample| {
        if s.source == vocmeet_core::Source::System && s.peak >= vocmeet_capture::SILENCE_PEAK {
            level_seen.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        let _ = level_app.emit(
            "recording-level",
            LevelEvent {
                source: format!("{:?}", s.source),
                rms: s.rms,
                peak: s.peak,
            },
        );
    });

    let stop = vocmeet_capture::StopSignal::new();

    // ---- 会中实时转写 ----
    //
    // 采集是实时任务，绝不能被转写堵住。所以走有界队列：
    // 满了就丢最旧的段并计数，丢掉的部分由会后那次转写兜住（pre_asr 为空即全量重算）。
    let live_on = cfg.capture.live_transcribe;
    let live_dropped = Arc::new(AtomicU64::new(0));
    let (seg_tx, seg_rx) = std::sync::mpsc::channel::<vocmeet_capture::LiveSegment>();
    let queued = Arc::new(AtomicU64::new(0));

    let segment_sink: Option<vocmeet_capture::SegmentSink> = if live_on {
        let q = queued.clone();
        let dropped = live_dropped.clone();
        Some(Arc::new(move |seg: vocmeet_capture::LiveSegment| {
            // 队列深度自己数：转写跟不上就直接丢，不阻塞采集线程。
            if q.load(Ordering::Relaxed) >= LIVE_QUEUE_LIMIT {
                dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            q.fetch_add(1, Ordering::Relaxed);
            let _ = seg_tx.send(seg);
        }))
    } else {
        None
    };

    let live_handle = if live_on {
        let app_live = app.clone();
        let q = queued.clone();
        let models_dir = cfg.models_dir.clone();
        let engine_opts = cfg.engine.to_options();
        Some(std::thread::spawn(move || {
            // 引擎加载一次整场复用（实测 0.7s）。加载失败不该让录制挂掉——
            // 实时稿只是加分项，录音本身与会后转写都不依赖它。
            let engine = match SherpaEngine::new(&ModelSet::from_root(&models_dir), engine_opts) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("实时转写引擎加载失败，本场退回会后整轨转写：{e}");
                    let _ = app_live.emit(
                        "live-transcript-off",
                        RecordingWarningEvent {
                            meeting_id,
                            kind: "live-engine-failed".into(),
                            message: format!("实时转写不可用（{e}），会后仍会完整转写。"),
                        },
                    );
                    // 把队列排空，免得 sink 那边一直累积计数。
                    while seg_rx.recv().is_ok() {
                        q.fetch_sub(1, Ordering::Relaxed);
                    }
                    return;
                }
            };
            let cancel = CancelToken::new();

            // 每轨各自的跨段缓冲：贴着段尾、可能被切断的那截音频，
            // 拼到下一段前面再转。两轨是两个独立的时间轴，不能共用。
            let mut carry: std::collections::HashMap<String, (audio::Pcm, u32)> =
                std::collections::HashMap::new();

            // 采集线程收尾后 channel 断开，这里自然退出。
            // 退出前还要把两轨各自剩下的尾巴转掉，否则最后一句永远不出现。
            let drain_tail = |engine: &SherpaEngine,
                              carry: &mut std::collections::HashMap<String, (audio::Pcm, u32)>,
                              app: &AppHandle| {
                for (key, (pcm, start)) in carry.drain() {
                    let source = if key == "Mic" {
                        vocmeet_core::Source::Mic
                    } else {
                        vocmeet_core::Source::System
                    };
                    // hold_tail = false：没有下一段了，全部转完
                    if let Ok(out) = engine.transcribe_segment(&pcm, source, start, false, &cancel) {
                        if out.segments.is_empty() {
                            continue;
                        }
                        let lines: Vec<LiveLine> = out
                            .segments
                            .iter()
                            .map(|s| LiveLine {
                                start_ms: s.start_ms,
                                source: key.clone(),
                                text: s.text.clone(),
                            })
                            .collect();
                        let _ = app.emit("live-transcript", LiveTranscriptEvent { meeting_id, lines });
                    }
                }
            };

            while let Ok(seg) = seg_rx.recv() {
                q.fetch_sub(1, Ordering::Relaxed);
                let source = seg.source;
                let key = format!("{source:?}");

                // 把上一轮留下的尾巴拼到本段前面，时间原点随之前移。
                let (input, offset) = match carry.remove(&key) {
                    Some((prev, prev_start)) => {
                        let mut samples = prev.samples;
                        samples.extend_from_slice(&seg.pcm.samples);
                        (
                            audio::Pcm {
                                sample_rate: seg.pcm.sample_rate,
                                samples,
                            },
                            prev_start,
                        )
                    }
                    None => (seg.pcm, seg.start_ms),
                };

                match engine.transcribe_segment(&input, source, offset, true, &cancel) {
                    Ok(out) => {
                        if let Some(c) = out.carry {
                            carry.insert(key.clone(), (c, out.carry_start_ms));
                        }
                        if out.segments.is_empty() {
                            continue;
                        }
                        let lines: Vec<LiveLine> = out
                            .segments
                            .iter()
                            .map(|s| LiveLine {
                                start_ms: s.start_ms,
                                source: key.clone(),
                                text: s.text.clone(),
                            })
                            .collect();
                        let _ = app_live.emit(
                            "live-transcript",
                            LiveTranscriptEvent { meeting_id, lines },
                        );
                    }
                    Err(e) => tracing::warn!("实时转写一段失败（会后会补上）：{e}"),
                }
            }

            drain_tail(&engine, &mut carry, &app_live);
        }))
    } else {
        None
    };

    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        vocmeet_capture::record_dual_track_with_levels(
            &capture_cfg,
            &stop_thread,
            Some(sink),
            segment_sink,
        )
    });

    // 看门狗：系统轨持续静默就提前告警，不等录完。
    //
    // 判断必须由挂钟驱动而不是由电平事件驱动——macOS 上 tap 在完全没有音频流过时
    // 根本不触发回调，那种情况下一个电平事件都不会来，事件驱动的检测永远不会响。
    let watchdog_stop = Arc::new(AtomicBool::new(false));
    let watchdog = if record_system {
        let flag = watchdog_stop.clone();
        let seen = last_audio_ms.clone();
        let wd_app = app.clone();
        Some(std::thread::spawn(move || {
            let mut fired = false;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if fired || flag.load(Ordering::Relaxed) {
                    continue;
                }
                let elapsed = started.elapsed();
                let quiet_since = Duration::from_millis(seen.load(Ordering::Relaxed));
                if elapsed.saturating_sub(quiet_since) >= SILENCE_ALERT_AFTER {
                    let _ = wd_app.emit(
                        "recording-warning",
                        RecordingWarningEvent {
                            meeting_id,
                            kind: "system-silent".into(),
                            // 与录制结束后的告警共用同一份文案。
                            message: vocmeet_capture::silence_warning(),
                        },
                    );
                    fired = true;
                }
            }
        }))
    } else {
        None
    };

    *state.recording.lock().map_err(|_| "录制锁中毒")? = Some(RecordingSession {
        meeting_id,
        stop,
        handle,
        watchdog_stop,
        watchdog,
        live: live_handle,
        live_dropped,
    });

    set_tray_recording(&app, true);

    Ok(meeting_id)
}

#[derive(Serialize)]
struct StopResult {
    meeting_id: i64,
    mic_chunks: usize,
    system_chunks: usize,
    warnings: Vec<String>,
    duration_ms: i64,
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) -> R<StopResult> {
    let session = state
        .recording
        .lock()
        .map_err(|_| "录制锁中毒")?
        .take()
        .ok_or("当前没有正在进行的录制")?;

    // 托盘先灭。后面 join 采集线程会阻塞住这个命令，
    // 拖到那之后再改图标，用户会看到菜单栏还在「录制中」。
    set_tray_recording(&app, false);

    session.watchdog_stop.store(true, Ordering::Relaxed);
    if let Some(w) = session.watchdog {
        let _ = w.join();
    }

    session.stop.stop();
    let rec = session
        .handle
        .join()
        .map_err(|_| "录制线程 panic".to_string())?
        .map_err(err)?;

    // 采集线程已经收尾（含 tap.flush 发出的最后一段），sink 随之释放，
    // channel 断开，实时转写线程自然退出。在这里 join 才能拿到**完整**的段集合——
    // 提前取的话最后十几秒会漏掉。
    if let Some(h) = session.live {
        let _ = h.join();
    }
    let dropped = session.live_dropped.load(Ordering::Relaxed);

    let duration_ms = rec
        .mic_chunks
        .iter()
        .chain(rec.system_chunks.iter())
        .map(|c| (c.start_ms + c.duration_ms) as i64)
        .max()
        .unwrap_or(0);

    let store = state.store()?;
    store
        .finish_meeting(session.meeting_id, &now_iso(), duration_ms)
        .map_err(err)?;

    // 合成一条可回放的音轨：转写用分轨，回放要听到完整现场。
    let playback = build_playback(&cfg_audio_dir(&state)?, session.meeting_id, &rec)
        .unwrap_or_else(|e| {
            tracing::warn!("生成回放音轨失败: {e}");
            None
        });
    if let Some(path) = &playback {
        let _ = store.set_playback_path(session.meeting_id, path);
    }

    // 实时稿只服务于「会中看得见」，会后一律整轨重算，不复用会中结果。
    //
    // 这是实测后的决定：188s 音频上整轨 111s，其中 VAD+ASR 只占 14.7s，
    // 说话人分离与标点占掉 94s。复用最多省 15%，却要承担分段边界识别变差的风险
    // （实测「说话人分离」被切成两段后认成「多话人分离」）。不划算。
    let mut warnings = rec.warnings.clone();
    if dropped > 0 {
        tracing::info!("实时转写有 {dropped} 段没跟上被丢弃（只影响会中显示）");
    }

    let result = StopResult {
        meeting_id: session.meeting_id,
        mic_chunks: rec.mic_chunks.len(),
        system_chunks: rec.system_chunks.len(),
        warnings: std::mem::take(&mut warnings),
        duration_ms,
    };

    *state.last_chunks.lock().map_err(|_| "分片锁中毒")? =
        Some((rec.mic_chunks, rec.system_chunks));

    Ok(result)
}

fn cfg_audio_dir(state: &State<AppState>) -> R<PathBuf> {
    Ok(state.config()?.audio_dir())
}

/// 把两条轨拼接、混音、落一个 playback.wav。失败不致命——只是没法回放。
fn build_playback(
    audio_dir: &std::path::Path,
    meeting_id: i64,
    rec: &vocmeet_capture::Recording,
) -> Result<Option<String>, String> {
    if rec.mic_chunks.is_empty() && rec.system_chunks.is_empty() {
        return Ok(None);
    }
    let mic = if rec.mic_chunks.is_empty() {
        audio::Pcm::default()
    } else {
        audio::concat_chunks(&rec.mic_chunks).map_err(err)?
    };
    let sys = if rec.system_chunks.is_empty() {
        audio::Pcm::default()
    } else {
        audio::concat_chunks(&rec.system_chunks).map_err(err)?
    };
    let mixed = audio::mix_tracks(&mic, &sys).map_err(err)?;
    if mixed.samples.is_empty() {
        return Ok(None);
    }
    let path = audio_dir
        .join(format!("meeting_{meeting_id}"))
        .join("playback.wav");
    audio::write_wav(&path, &mixed).map_err(err)?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
fn recording_status(state: State<AppState>) -> R<Option<i64>> {
    Ok(state
        .recording
        .lock()
        .map_err(|_| "录制锁中毒")?
        .as_ref()
        .map(|s| s.meeting_id))
}

// ---------------------------------------------------------------- 命令：转写

#[tauri::command]
fn transcribe(
    app: AppHandle,
    state: State<AppState>,
    meeting_id: i64,
    speakers: Option<i32>,
    mic_wav: Option<String>,
    system_wav: Option<String>,
) -> R<()> {
    if state
        .transcribe_cancel
        .lock()
        .map_err(|_| "任务锁中毒")?
        .is_some()
    {
        return Err("已有转写任务在进行中（MVP 为单任务串行）".into());
    }

    let cfg = state.config()?;
    let key = state.master_key.clone();

    // 音轨来源：显式传入的 WAV 优先，否则用上一次录制的分片。
    let chunks = state
        .last_chunks
        .lock()
        .map_err(|_| "分片锁中毒")?
        .clone();

    let cancel = CancelToken::new();
    *state.transcribe_cancel.lock().map_err(|_| "任务锁中毒")? = Some(cancel.clone());

    let app_for_thread = app.clone();
    std::thread::spawn(move || {
        let result = run_transcription(
            &app_for_thread,
            &cfg,
            &key,
            meeting_id,
            speakers,
            mic_wav.map(PathBuf::from),
            system_wav.map(PathBuf::from),
            chunks,
            &cancel,
        );

        let done = match result {
            Ok(n) => DoneEvent {
                meeting_id,
                ok: true,
                message: format!("转写完成，{n} 条发言"),
                utterance_count: n,
            },
            Err(e) => DoneEvent {
                meeting_id,
                ok: false,
                message: e,
                utterance_count: 0,
            },
        };
        let _ = app_for_thread.emit("transcribe-done", done);

        if let Some(state) = app_for_thread.try_state::<AppState>() {
            if let Ok(mut g) = state.transcribe_cancel.lock() {
                *g = None;
            }
        }
    });

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_transcription(
    app: &AppHandle,
    cfg: &Config,
    key: &[u8],
    meeting_id: i64,
    speakers: Option<i32>,
    mic_wav: Option<PathBuf>,
    system_wav: Option<PathBuf>,
    chunks: Option<(Vec<ChunkInfo>, Vec<ChunkInfo>)>,
    cancel: &CancelToken,
) -> Result<usize, String> {
    let emit = |stage: Stage, fraction: f32, detail: &str| {
        let _ = app.emit(
            "transcribe-progress",
            ProgressEvent {
                meeting_id,
                stage: format!("{stage:?}"),
                progress: overall_progress(stage, fraction),
                detail: detail.to_string(),
            },
        );
    };

    emit(Stage::LoadingModels, 0.0, "加载模型");
    let models = ModelSet::from_root(&cfg.models_dir);
    let engine = SherpaEngine::new(&models, cfg.engine.to_options()).map_err(err)?;
    emit(Stage::LoadingModels, 1.0, "模型就绪");

    // 组装两条轨：显式 WAV > 录制分片。
    let load = |p: Option<PathBuf>, cs: Option<&Vec<ChunkInfo>>| -> Result<Option<audio::Pcm>, String> {
        if let Some(path) = p {
            let pcm = audio::read_wav(&path).map_err(err)?;
            // 已经是目标采样率就直接用：resample 会整段复制一次，
            // 而导入进来的音轨动辄几百 MB，这一次复制省得掉。
            if pcm.sample_rate == TARGET_SAMPLE_RATE {
                return Ok(Some(pcm));
            }
            return Ok(Some(audio::resample_to_target(&pcm).map_err(err)?));
        }
        if let Some(list) = cs {
            if !list.is_empty() {
                return Ok(Some(audio::concat_chunks(list).map_err(err)?));
            }
        }
        Ok(None)
    };

    let (mic_chunks, sys_chunks) = match &chunks {
        Some((m, s)) => (Some(m), Some(s)),
        None => (None, None),
    };

    let job = TranscribeJob {
        mic: load(mic_wav, mic_chunks)?,
        system: load(system_wav, sys_chunks)?,
        known_speakers: speakers,
    };

    if job.mic.is_none() && job.system.is_none() {
        return Err("没有可转写的音频：既没有录制分片，也没有指定 WAV 文件".into());
    }

    let progress_cb = |p: Progress| {
        emit(p.stage, p.fraction, &p.detail);
    };

    let utterances = engine
        .transcribe(&job, &progress_cb, cancel)
        .map_err(|e| e.to_string())?;

    let store = Store::open(cfg.db_path(), key).map_err(err)?;
    store
        .replace_utterances(meeting_id, &utterances)
        .map_err(err)?;
    store
        .set_meeting_status(meeting_id, "transcribed")
        .map_err(err)?;

    // 逐字稿自动落盘到配置的落盘目录，并记录绝对地址
    let transcripts_dir = cfg.transcripts_dir();
    let _ = std::fs::create_dir_all(&transcripts_dir);
    let title = store
        .get_meeting(meeting_id)
        .ok()
        .flatten()
        .map(|m| m.title)
        .unwrap_or_else(|| format!("会议_{meeting_id}"));
    let safe_title: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' || ('\u{4e00}'..='\u{9fa5}').contains(&c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let transcript_file = transcripts_dir.join(format!("{safe_title}_{meeting_id}_逐字稿.md"));
    let mut md = format!("# 会议逐字稿 - {}\n\n", title);
    md.push_str(&format!("- 会议编号：{}\n", meeting_id));
    md.push_str(&format!("- 生成时间：{}\n", now_iso()));
    md.push_str(&format!("- 发言条数：{}\n\n", utterances.len()));
    for u in &utterances {
        let mark = if u.low_confidence { " *(识别存疑)*" } else { "" };
        md.push_str(&format!(
            "**[{}]** `{}`：{}{}\n\n",
            u.display_speaker(),
            format_ts(u.start_ms),
            u.text,
            mark
        ));
    }
    if let Ok(()) = std::fs::write(&transcript_file, md) {
        let _ = store.set_transcript_path(meeting_id, &transcript_file.display().to_string());
    }

    Ok(utterances.len())
}

#[derive(Serialize, Clone)]
struct ProcessEvent {
    meeting_id: i64,
    /// importing | transcribing | summarizing | done | failed
    phase: String,
    progress: f32,
    detail: String,
}

/// 结束会议后的一条龙处理：转写 → 纪要 → 关键词。
///
/// 产品上刻意不暴露「排队中」这类内部状态——用户只需要知道「会议已完成，正在整理」。
/// 纪要失败不回滚逐字稿：逐字稿本身就有价值，缺的只是纪要，可以稍后重试。
#[tauri::command]
fn process_meeting(
    app: AppHandle,
    state: State<AppState>,
    meeting_id: i64,
    speakers: Option<i32>,
) -> R<()> {
    if state
        .transcribe_cancel
        .lock()
        .map_err(|_| "任务锁中毒")?
        .is_some()
    {
        return Err("上一场会议还在整理中".into());
    }

    let cfg = state.config()?;
    let key = state.master_key.clone();
    let chunks = state.last_chunks.lock().map_err(|_| "分片锁中毒")?.clone();

    let cancel = CancelToken::new();
    *state.transcribe_cancel.lock().map_err(|_| "任务锁中毒")? = Some(cancel.clone());

    if let Ok(store) = state.store() {
        let _ = store.set_meeting_status(meeting_id, "processing");
    }

    std::thread::spawn(move || {
        let emit = |phase: &str, progress: f32, detail: &str| {
            let _ = app.emit(
                "process-progress",
                ProcessEvent {
                    meeting_id,
                    phase: phase.to_string(),
                    progress,
                    detail: detail.to_string(),
                },
            );
        };

        let outcome = (|| -> Result<usize, String> {
            emit("transcribing", 0.0, "整理逐字稿");
            let n = run_transcription(
                &app, &cfg, &key, meeting_id, speakers, None, None, chunks, &cancel,
            )?;
            emit("summarizing", 0.0, "整理纪要");
            // 纪要与关键词失败都不算整场失败。
            match run_summarize(&app, &cfg, &key, meeting_id) {
                Ok(_) => {}
                Err(e) => tracing::warn!("纪要生成失败: {e}"),
            }
            Ok(n)
        })();

        finish_processing(&app, &cfg, &key, meeting_id, outcome);
    });

    Ok(())
}

#[tauri::command]
fn cancel_transcribe(state: State<AppState>) -> R<()> {
    if let Some(c) = state
        .transcribe_cancel
        .lock()
        .map_err(|_| "任务锁中毒")?
        .as_ref()
    {
        c.cancel();
    }
    Ok(())
}



// ---------------------------------------------------------------- 命令：模型与端点

#[derive(Serialize, Clone)]
struct ModelFetchEvent {
    phase: String,
    index: usize,
    total_assets: usize,
    label: String,
    received: u64,
    total: Option<u64>,
    overall: f32,
}

/// 下载前告诉用户要下多少、下到哪。
#[derive(Serialize)]
struct ModelPlan {
    models_dir: String,
    total_bytes: u64,
    assets: Vec<String>,
    already_present: bool,
}

#[tauri::command]
fn model_download_plan(state: State<AppState>) -> R<ModelPlan> {
    let cfg = state.config()?;
    Ok(ModelPlan {
        already_present: ModelSet::from_root(&cfg.models_dir).verify_present().is_ok(),
        models_dir: cfg.models_dir.display().to_string(),
        total_bytes: vocmeet_core::models::total_download_bytes(),
        assets: vocmeet_core::models::ASSETS
            .iter()
            .map(|a| a.label.to_string())
            .collect(),
    })
}

/// 一键下载全部权重。进度走 `models-progress`，收尾走 `models-done`。
///
/// 下载完直接写回配置——「下载了但还要自己去设置目录」不叫一键。
#[tauri::command]
fn download_models(app: AppHandle, state: State<AppState>) -> R<()> {
    if state
        .model_fetch_cancel
        .lock()
        .map_err(|_| "下载锁中毒")?
        .is_some()
    {
        return Err("已经在下载了".into());
    }

    let cfg = state.config()?;
    // 下到数据目录：Program Files 之下通常没有写权限，而数据目录一定有。
    let dest = if cfg.models_dir.starts_with(&cfg.data_dir) {
        cfg.models_dir.clone()
    } else {
        cfg.data_dir.join("models")
    };

    let cancel = CancelToken::new();
    *state.model_fetch_cancel.lock().map_err(|_| "下载锁中毒")? = Some(cancel.clone());

    std::thread::spawn(move || {
        let emit = |p: vocmeet_core::models::FetchProgress| {
            let _ = app.emit(
                "models-progress",
                ModelFetchEvent {
                    phase: format!("{:?}", p.phase).to_lowercase(),
                    index: p.index,
                    total_assets: p.total_assets,
                    label: p.label.to_string(),
                    received: p.received,
                    total: p.total,
                    overall: p.overall,
                },
            );
        };

        let outcome = tokio::runtime::Runtime::new()
            .map_err(|e| e.to_string())
            .and_then(|rt| {
                rt.block_on(vocmeet_core::models::fetch_all(
                    &dest,
                    &emit,
                    &|| cancel.is_cancelled(),
                ))
                .map_err(|e| e.to_string())
            });

        let message = match &outcome {
            Ok(()) => {
                // 生效：把目录写回配置，用户不用再去设置页点一遍。
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(mut guard) = state.config.lock() {
                        guard.models_dir = dest.clone();
                        let _ = guard.save(&state.config_path);
                    }
                }
                format!("模型已就位：{}", dest.display())
            }
            Err(e) => e.clone(),
        };

        let _ = app.emit(
            "models-done",
            DoneEvent {
                meeting_id: 0,
                ok: outcome.is_ok(),
                message,
                utterance_count: 0,
            },
        );

        if let Some(state) = app.try_state::<AppState>() {
            if let Ok(mut g) = state.model_fetch_cancel.lock() {
                *g = None;
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn cancel_model_download(state: State<AppState>) -> R<()> {
    if let Some(c) = state
        .model_fetch_cancel
        .lock()
        .map_err(|_| "下载锁中毒")?
        .as_ref()
    {
        c.cancel();
    }
    Ok(())
}

/// 手动指定一个已有的权重目录（比如从别的机器拷过来的）。
#[tauri::command]
fn set_models_dir(state: State<AppState>, path: String) -> R<String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("这不是一个目录：{path}"));
    }
    let set = ModelSet::from_root(&dir);
    set.verify_present().map_err(err)?;

    let mut guard = state.config.lock().map_err(|_| "配置锁中毒")?;
    guard.models_dir = dir;
    guard.save(&state.config_path).map_err(err)?;
    Ok(format!("已指向 {path}"))
}

/// 修改逐字稿落盘目录。修改不影响已有旧会议的逐字稿路径。
#[tauri::command]
fn set_transcripts_dir(state: State<AppState>, path: String) -> R<String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir).map_err(err)?;
    }
    let mut guard = state.config.lock().map_err(|_| "配置锁中毒")?;
    guard.transcripts_dir = Some(dir);
    guard.save(&state.config_path).map_err(err)?;
    Ok(format!("已修改逐字稿落盘目录：{path}"))
}

/// 参会人管理：列出全局所有会议出现过的参会人聚合列表。
#[tauri::command]
fn list_all_participants(state: State<AppState>) -> R<Vec<vocmeet_core::store::ParticipantInfo>> {
    state.store()?.list_all_participants().map_err(err)
}

/// LLM 端点体检。分清「没起来」「没模型」「模型名不对」。
#[tauri::command]
async fn diagnose_llm(state: State<'_, AppState>) -> R<vocmeet_core::llm::LlmDiagnosis> {
    let cfg = state.config()?;
    let client = LlmClient::new(cfg.llm.clone(), cfg.egress_policy, store::get_api_key())
        .map_err(err)?;
    Ok(client.diagnose().await)
}


#[derive(Serialize, Clone)]
struct PullEvent {
    status: String,
    completed: Option<u64>,
    total: Option<u64>,
}

/// 让本机 Ollama 拉一个模型。进度走 `llm-pull-progress`，收尾走 `llm-pull-done`。
///
/// 拉完把模型名写回配置——和识别模型一样，一键就该是一键。
#[tauri::command]
fn pull_llm_model(app: AppHandle, state: State<AppState>, model: String) -> R<()> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("先填一个模型名".into());
    }
    if state.llm_pulling.lock().map_err(|_| "拉取锁中毒")?.clone() {
        return Err("已经在拉了".into());
    }
    let cfg = state.config()?;
    *state.llm_pulling.lock().map_err(|_| "拉取锁中毒")? = true;

    std::thread::spawn(move || {
        let outcome = (|| -> Result<(), String> {
            let client = LlmClient::new(cfg.llm.clone(), cfg.egress_policy, store::get_api_key())
                .map_err(err)?;
            let rt = tokio::runtime::Runtime::new().map_err(err)?;
            rt.block_on(client.pull_model(&model, &|p| {
                let _ = app.emit(
                    "llm-pull-progress",
                    PullEvent {
                        status: p.status,
                        completed: p.completed,
                        total: p.total,
                    },
                );
            }))
            .map_err(err)
        })();

        let message = match &outcome {
            Ok(()) => {
                if let Some(state) = app.try_state::<AppState>() {
                    if let Ok(mut guard) = state.config.lock() {
                        guard.llm.model = model.clone();
                        let _ = guard.save(&state.config_path);
                    }
                }
                format!("{model} 已就位，会议总结LLM已切到它")
            }
            Err(e) => e.clone(),
        };
        let _ = app.emit(
            "llm-pull-done",
            DoneEvent {
                meeting_id: 0,
                ok: outcome.is_ok(),
                message,
                utterance_count: 0,
            },
        );

        if let Some(state) = app.try_state::<AppState>() {
            if let Ok(mut g) = state.llm_pulling.lock() {
                *g = false;
            }
        }
    });

    Ok(())
}

/// 推荐给「一个模型都没有」的用户的几个选项。
///
/// 都是能在 16GB 内存机器上跑起来的中文友好模型；`context_tokens` 直接决定
/// map-reduce 的分块大小（§4.4），所以一并给出，选完就写进配置。
#[tauri::command]
fn suggested_llm_models() -> Vec<(String, String, usize)> {
    vec![
        (
            "qwen2.5:7b".into(),
            "约 4.7GB · 中文纪要质量与体积的平衡点，推荐".into(),
            8192,
        ),
        (
            "qwen2.5:3b".into(),
            "约 1.9GB · 机器吃紧时用，纪要会更粗".into(),
            8192,
        ),
        (
            "qwen2.5:14b".into(),
            "约 9GB · 显存够就上这个，长会议明显更稳".into(),
            16384,
        ),
    ]
}

// ---------------------------------------------------------------- 命令：导入音频

/// 能导入的扩展名。清单的唯一真源在 core，前端不另维护一份。
#[tauri::command]
fn importable_extensions() -> Vec<String> {
    audio::IMPORTABLE_EXTENSIONS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 导入一个音频文件，后续走与录制完全相同的流程：解码 → 转写 → 纪要。
///
/// 解出来的整条音轨当作**系统轨**：导入的录音里所有人都在同一条轨上，
/// 没有「本机用户」可言，麦克风轨那条免 diarization 的捷径用不上（§4.1 双轨策略）。
#[tauri::command]
fn import_audio(
    app: AppHandle,
    state: State<AppState>,
    path: String,
    title: Option<String>,
    speakers: Option<i32>,
) -> R<i64> {
    if state.recording.lock().map_err(|_| "录制锁中毒")?.is_some() {
        return Err("正在录制，先结束当前会议再导入".into());
    }
    if state
        .transcribe_cancel
        .lock()
        .map_err(|_| "任务锁中毒")?
        .is_some()
    {
        return Err("上一场会议还在整理中".into());
    }

    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("找不到这个文件：{path}"));
    }
    if !audio::is_importable(&src) {
        return Err(format!(
            "认不出这种格式。能导入的是：{}",
            audio::IMPORTABLE_EXTENSIONS.join(" / ")
        ));
    }

    // 没给标题就用文件名——拖进来的文件名通常就是用户心里的会议名。
    let title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            src.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "导入的音频".to_string());

    let cfg = state.config()?;
    let key = state.master_key.clone();
    let store = state.store()?;
    let meeting_id = store.create_meeting(&title, &now_iso()).map_err(err)?;
    let _ = store.set_meeting_status(meeting_id, "processing");

    let cancel = CancelToken::new();
    *state.transcribe_cancel.lock().map_err(|_| "任务锁中毒")? = Some(cancel.clone());

    std::thread::spawn(move || {
        let emit = |phase: &str, progress: f32, detail: &str| {
            let _ = app.emit(
                "process-progress",
                ProcessEvent {
                    meeting_id,
                    phase: phase.to_string(),
                    progress,
                    detail: detail.to_string(),
                },
            );
        };

        let outcome = (|| -> Result<usize, String> {
            emit("importing", 0.0, "读取音频");
            // 转写和回放共用这一个文件：解出来的 16k 单声道 WAV 两边都够用，
            // 也省得让 webview 去啃 .aac 这类它未必认识的容器。
            let wav = cfg
                .audio_dir()
                .join(format!("meeting_{meeting_id}"))
                .join("playback.wav");

            let duration_ms = audio::transcode_to_wav(&src, &wav, &|p| {
                let detail = match p.total_ms {
                    Some(total) => format!(
                        "解码 {} / {}",
                        format_ts(p.decoded_ms as u32),
                        format_ts(total as u32)
                    ),
                    None => format!("解码 {}", format_ts(p.decoded_ms as u32)),
                };
                emit("importing", p.fraction().unwrap_or(0.0), &detail);
            })
            .map_err(|e| e.to_string())?;

            let store = Store::open(cfg.db_path(), &key).map_err(err)?;
            store
                .finish_meeting(meeting_id, &now_iso(), duration_ms as i64)
                .map_err(err)?;
            // finish_meeting 把状态写成了 recorded，但这场会才刚开始整理。
            let _ = store.set_meeting_status(meeting_id, "processing");
            let _ = store.set_playback_path(meeting_id, &wav.display().to_string());

            emit("transcribing", 0.0, "整理逐字稿");
            let n = run_transcription(
                &app,
                &cfg,
                &key,
                meeting_id,
                speakers,
                None,
                Some(wav),
                None,
                &cancel,
            )?;

            emit("summarizing", 0.0, "整理纪要");
            // 与录制流程一致：纪要失败不算整场失败，逐字稿本身就有价值。
            if let Err(e) = run_summarize(&app, &cfg, &key, meeting_id) {
                tracing::warn!("纪要生成失败: {e}");
            }
            Ok(n)
        })();

        finish_processing(&app, &cfg, &key, meeting_id, outcome);
    });

    Ok(meeting_id)
}

/// 整理流程的收尾：落状态、推 `process-done`、放掉任务锁。
///
/// 录制与导入两条路走到最后是同一件事，这里收口，免得两边各写一遍再慢慢跑偏。
fn finish_processing(
    app: &AppHandle,
    cfg: &Config,
    key: &[u8],
    meeting_id: i64,
    outcome: Result<usize, String>,
) {
    let store = Store::open(cfg.db_path(), key).ok();
    let ev = match &outcome {
        Ok(n) => {
            if let Some(s) = &store {
                let _ = s.set_meeting_status(meeting_id, "done");
            }
            ProcessEvent {
                meeting_id,
                phase: "done".into(),
                progress: 1.0,
                detail: format!("{n} 条发言"),
            }
        }
        Err(e) => {
            if let Some(s) = &store {
                let _ = s.set_meeting_status(meeting_id, "failed");
            }
            ProcessEvent {
                meeting_id,
                phase: "failed".into(),
                progress: 0.0,
                detail: e.clone(),
            }
        }
    };
    let _ = app.emit("process-done", ev);

    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut g) = state.transcribe_cancel.lock() {
            *g = None;
        }
    }
}


/// 一场会议留在盘上的音源。重新解析时用它，不依赖任何内存状态。
struct MeetingAudio {
    mic: Vec<ChunkInfo>,
    system: Vec<ChunkInfo>,
    /// 没有分片时的回落：整条回放轨（导入进来的会议就只有这个）。
    single_wav: Option<PathBuf>,
}

impl MeetingAudio {
    fn is_empty(&self) -> bool {
        self.mic.is_empty() && self.system.is_empty() && self.single_wav.is_none()
    }
}

/// 找出这场会议还能拿来重跑的音频。
///
/// 优先用分片：录制是双轨的，麦克风轨不做 diarization（§4.1），
/// 拿混音后的 playback.wav 重跑会把这个区分丢掉。分片没了才退回 playback。
fn locate_meeting_audio(cfg: &Config, meeting_id: i64, playback: Option<&str>) -> MeetingAudio {
    let dir = cfg.audio_dir().join(format!("meeting_{meeting_id}"));
    let mic = audio::scan_chunks(&dir, "mic").unwrap_or_default();
    let system = audio::scan_chunks(&dir, "sys").unwrap_or_default();

    let single_wav = if mic.is_empty() && system.is_empty() {
        playback
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                let p = dir.join("playback.wav");
                p.is_file().then_some(p)
            })
    } else {
        None
    };

    MeetingAudio {
        mic,
        system,
        single_wav,
    }
}

/// 这场会议还能不能重新解析。UI 据此决定要不要给那个按钮。
#[tauri::command]
fn meeting_has_audio(state: State<AppState>, meeting_id: i64) -> R<bool> {
    let cfg = state.config()?;
    let playback = state.store()?.get_playback_path(meeting_id).map_err(err)?;
    Ok(!locate_meeting_audio(&cfg, meeting_id, playback.as_deref()).is_empty())
}

/// 重新整理一场已有的会议。
///
/// `mode`：
/// - `summary` —— 只重写纪要，逐字稿原样不动。改了模型或者对这版纪要不满意时用。
/// - `full`    —— 从盘上的录音重跑：识别 → 说话人分离 → 纪要。
///
/// 两种都用**触发时**的配置：模型、端点、聚类阈值、出网策略，都读当前值。
/// 换个 LLM 再点一次，就是换个模型重写一遍——这正是这个按钮存在的理由。
///
/// 旧纪要不会被删：`save_summary` 是追加，历史都留着，界面显示最新的那版。
#[tauri::command]
fn reprocess_meeting(
    app: AppHandle,
    state: State<AppState>,
    meeting_id: i64,
    mode: String,
    speakers: Option<i32>,
) -> R<()> {
    if state
        .transcribe_cancel
        .lock()
        .map_err(|_| "任务锁中毒")?
        .is_some()
    {
        return Err("还有一场会议在整理中，等它完成".into());
    }
    let full = match mode.as_str() {
        "full" => true,
        "summary" => false,
        other => return Err(format!("不认识的重整方式：{other}")),
    };

    let cfg = state.config()?;
    let key = state.master_key.clone();
    let store = state.store()?;
    store.get_meeting(meeting_id).map_err(err)?.ok_or("会议不存在")?;

    let audio = if full {
        let playback = store.get_playback_path(meeting_id).map_err(err)?;
        let found = locate_meeting_audio(&cfg, meeting_id, playback.as_deref());
        if found.is_empty() {
            return Err("这场会议的录音已经不在盘上了，只能重写纪要".into());
        }
        Some(found)
    } else {
        if store.load_utterances(meeting_id).map_err(err)?.is_empty() {
            return Err("这场会议还没有逐字稿，先重新解析录音".into());
        }
        None
    };

    let cancel = CancelToken::new();
    *state.transcribe_cancel.lock().map_err(|_| "任务锁中毒")? = Some(cancel.clone());
    let _ = store.set_meeting_status(meeting_id, "processing");

    std::thread::spawn(move || {
        let emit = |phase: &str, progress: f32, detail: &str| {
            let _ = app.emit(
                "process-progress",
                ProcessEvent {
                    meeting_id,
                    phase: phase.to_string(),
                    progress,
                    detail: detail.to_string(),
                },
            );
        };

        let outcome = (|| -> Result<usize, String> {
            let count;
            if let Some(audio) = audio {
                emit("transcribing", 0.0, "重新识别");
                let chunks = if audio.single_wav.is_some() {
                    None
                } else {
                    Some((audio.mic.clone(), audio.system.clone()))
                };
                count = run_transcription(
                    &app,
                    &cfg,
                    &key,
                    meeting_id,
                    speakers,
                    None,
                    audio.single_wav.clone(),
                    chunks,
                    &cancel,
                )?;
            } else {
                let store = Store::open(cfg.db_path(), &key).map_err(err)?;
                count = store.load_utterances(meeting_id).map_err(err)?.len();
            }
            emit("summarizing", 0.0, "重写纪要");
            // 与录制/导入不同，这里纪要失败**算失败**：用户点这个按钮就是为了纪要，
            // 悄悄跳过再报「完成」等于骗人。逐字稿已经落库了，不会白跑。
            if let Err(e) = run_summarize(&app, &cfg, &key, meeting_id) {
                return Err(if count > 0 {
                    format!("逐字稿已更新（{count} 条），但纪要没写成：{e}")
                } else {
                    e
                });
            }
            Ok(count)
        })();

        finish_processing(&app, &cfg, &key, meeting_id, outcome);
    });

    Ok(())
}

// ---------------------------------------------------------------- 命令：逐字稿与说话人

#[tauri::command]
fn get_transcript(state: State<AppState>, meeting_id: i64) -> R<Vec<Utterance>> {
    state.store()?.load_utterances(meeting_id).map_err(err)
}

#[tauri::command]
fn update_utterance(
    state: State<AppState>,
    meeting_id: i64,
    ordinal: i64,
    text: String,
) -> R<()> {
    state
        .store()?
        .update_utterance_text(meeting_id, ordinal, &text)
        .map_err(err)
}

#[tauri::command]
fn list_speakers(state: State<AppState>, meeting_id: i64) -> R<Vec<SpeakerRow>> {
    let store = state.store()?;
    let utterances = store.load_utterances(meeting_id).map_err(err)?;
    let names = store.load_speaker_names(meeting_id).map_err(err)?;

    let mut rows = Vec::new();
    for key in store.distinct_speakers(meeting_id).map_err(err)? {
        let mine: Vec<&Utterance> = utterances
            .iter()
            .filter(|u| u.speaker_id == key)
            .collect();
        // 取最长的一条作为代表性发言，比取第一条更有辨识度。
        let sample = mine
            .iter()
            .max_by_key(|u| u.text.chars().count())
            .map(|u| u.text.chars().take(60).collect::<String>())
            .unwrap_or_default();
        rows.push(SpeakerRow {
            display_name: names.get(&key).cloned(),
            key,
            sample,
            utterance_count: mine.len(),
        });
    }
    Ok(rows)
}

#[tauri::command]
fn name_speaker(
    state: State<AppState>,
    meeting_id: i64,
    speaker_key: String,
    display_name: String,
) -> R<()> {
    state
        .store()?
        .name_speaker(meeting_id, &speaker_key, &display_name)
        .map_err(err)
}

// ---------------------------------------------------------------- 命令：速记

#[tauri::command]
fn get_note(state: State<AppState>, meeting_id: i64) -> R<String> {
    state.store()?.load_note(meeting_id).map_err(err)
}

#[tauri::command]
fn save_note(state: State<AppState>, meeting_id: i64, content: String) -> R<()> {
    state.store()?.save_note(meeting_id, &content).map_err(err)
}

// ---------------------------------------------------------------- 命令：纪要

#[tauri::command]
fn summarize(app: AppHandle, state: State<AppState>, meeting_id: i64) -> R<()> {
    let cfg = state.config()?;
    let key = state.master_key.clone();

    std::thread::spawn(move || {
        let outcome = run_summarize(&app, &cfg, &key, meeting_id);
        let done = match outcome {
            Ok(msg) => DoneEvent {
                meeting_id,
                ok: true,
                message: msg,
                utterance_count: 0,
            },
            Err(e) => DoneEvent {
                meeting_id,
                ok: false,
                message: e,
                utterance_count: 0,
            },
        };
        let _ = app.emit("summarize-done", done);
    });

    Ok(())
}

fn run_summarize(
    app: &AppHandle,
    cfg: &Config,
    key: &[u8],
    meeting_id: i64,
) -> Result<String, String> {
    let store = Store::open(cfg.db_path(), key).map_err(err)?;
    let utterances = store.load_utterances(meeting_id).map_err(err)?;
    if utterances.is_empty() {
        return Err("这场会议还没有逐字稿".into());
    }
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or("会议不存在")?;
    let scratchpad = store.load_note(meeting_id).map_err(err)?;

    let template_path = cfg.templates_dir.join("meeting_default.yaml");
    let template = Template::load(&template_path).map_err(err)?;

    // 统计每位发言人的发言条数，按活跃频次降序排列
    let mut speaker_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for u in &utterances {
        *speaker_counts.entry(u.display_speaker().to_string()).or_default() += 1;
    }
    let mut sorted_speakers: Vec<String> = speaker_counts.keys().cloned().collect();
    sorted_speakers.sort_by(|a, b| speaker_counts[b].cmp(&speaker_counts[a]));

    // 参会人防爆炸保护：参会人过多时（聚类过分裂可能达数百人），
    // 仅保留发言最活跃的前8位核心发言人加"等共N人"，绝不让模型耗尽token去复读几百个代号
    let total_speakers = sorted_speakers.len();
    let attendees: Vec<String> = if total_speakers > 8 {
        let mut top: Vec<String> = sorted_speakers.into_iter().take(8).collect();
        top.push(format!("等共 {} 人", total_speakers));
        top
    } else {
        sorted_speakers
    };

    // 接入设置中的预设信息：行业专用词库与系统补充提示词
    let mut scratchpad_with_presets = scratchpad.clone();
    if !cfg.preset_terms.is_empty() {
        scratchpad_with_presets.push_str(&format!(
            "\n\n【行业专用词库（重点识别参考）】：{}",
            cfg.preset_terms.join("、")
        ));
    }
    if !cfg.preset_prompt.trim().is_empty() {
        scratchpad_with_presets.push_str(&format!(
            "\n\n【用户补充指令】：{}",
            cfg.preset_prompt.trim()
        ));
    }

    let client = LlmClient::new(cfg.llm.clone(), cfg.egress_policy, store::get_api_key())
        .map_err(err)?;

    let input = RenderInput {
        meeting_title: &meeting.title,
        meeting_time: &meeting.started_at,
        attendees: &attendees,
        scratchpad: &scratchpad_with_presets,
        utterances: &utterances,
    };
    let summarizer = Summarizer {
        client: &client,
        template: &template,
        config: &cfg.llm,
        // §5：说话人真名默认不出本机。
        redact_names: true,
    };

    let rt = tokio::runtime::Runtime::new().map_err(err)?;
    let out = rt.block_on(async {
        let mut on_delta = |d: &str| {
            let _ = app.emit(
                "summarize-delta",
                DeltaEvent {
                    meeting_id,
                    delta: d.to_string(),
                },
            );
        };
        summarizer.run(&input, &mut on_delta).await
    });

    let out = out.map_err(|e| e.to_string())?;
    for rec in &out.egress {
        let _ = store.log_egress(Some(meeting_id), rec);
    }
    store
        .save_summary(meeting_id, &cfg.llm.model, &out.markdown)
        .map_err(err)?;

    // 关键词是纪要的派生物，失败不影响纪要本身。
    match rt.block_on(vocmeet_core::summarize::extract_keywords(&client, &out.markdown)) {
        Ok(kw) if !kw.is_empty() => {
            let _ = store.set_keywords(meeting_id, &kw);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("关键词提取失败: {e}"),
    }

    Ok(format!(
        "纪要已生成（{}，{} 次出网已记入审计）",
        if out.used_map_reduce {
            "map-reduce"
        } else {
            "单次生成"
        },
        out.egress.len()
    ))
}

#[tauri::command]
fn get_summary(state: State<AppState>, meeting_id: i64) -> R<Option<String>> {
    state.store()?.latest_summary(meeting_id).map_err(err)
}

// ---------------------------------------------------------------- 命令：导出与配置

#[tauri::command]
fn export_markdown(state: State<AppState>, meeting_id: i64, path: String) -> R<String> {
    let store = state.store()?;
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or("会议不存在")?;
    let utterances = store.load_utterances(meeting_id).map_err(err)?;
    let summary = store.latest_summary(meeting_id).map_err(err)?;

    let mut md = format!("# {}\n\n", meeting.title);
    md.push_str(&format!("- 开始时间：{}\n", meeting.started_at));
    md.push_str(&format!(
        "- 时长：{}\n",
        format_ts(meeting.duration_ms as u32)
    ));
    md.push_str(&format!("- 发言条数：{}\n\n", utterances.len()));

    if let Some(s) = summary {
        md.push_str("## 会议纪要\n\n");
        md.push_str(&s);
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

    let p = PathBuf::from(&path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    std::fs::write(&p, md).map_err(err)?;
    Ok(p.display().to_string())
}

#[tauri::command]
fn archive_meeting(state: State<AppState>, meeting_id: i64, archived: bool) -> R<()> {
    state.store()?.archive_meeting(meeting_id, archived).map_err(err)
}

#[tauri::command]
fn list_archived_meetings(state: State<AppState>) -> R<Vec<Meeting>> {
    state.store()?.list_archived_meetings().map_err(err)
}

#[tauri::command]
fn export_meeting_audio(state: State<AppState>, meeting_id: i64, target_path: String) -> R<String> {
    let cfg = state.config()?;
    let store = state.store()?;
    let playback = store.get_playback_path(meeting_id).map_err(err)?;
    let audio = locate_meeting_audio(&cfg, meeting_id, playback.as_deref());

    let src_path = if let Some(p) = audio.single_wav {
        p
    } else {
        let dir = cfg.audio_dir().join(format!("meeting_{meeting_id}"));
        let p = dir.join("playback.wav");
        if p.is_file() {
            p
        } else {
            return Err("未找到该会议的完整音频源文件".into());
        }
    };

    let target = PathBuf::from(&target_path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    std::fs::copy(&src_path, &target).map_err(err)?;
    Ok(target.display().to_string())
}

#[tauri::command]
fn export_transcript_markdown(state: State<AppState>, meeting_id: i64, target_path: String) -> R<String> {
    let store = state.store()?;
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or("会议不存在")?;
    let utterances = store.load_utterances(meeting_id).map_err(err)?;

    let mut md = format!("# {} - 逐字稿\n\n", meeting.title);
    md.push_str(&format!("- 开始时间：{}\n", meeting.started_at));
    md.push_str(&format!(
        "- 时长：{}\n",
        format_ts(meeting.duration_ms as u32)
    ));
    md.push_str(&format!("- 发言条数：{}\n\n", utterances.len()));

    md.push_str("## 逐字记录\n\n");
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

    let p = PathBuf::from(&target_path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    std::fs::write(&p, md).map_err(err)?;
    Ok(p.display().to_string())
}

#[tauri::command]
fn export_summary_markdown(state: State<AppState>, meeting_id: i64, target_path: String) -> R<String> {
    let store = state.store()?;
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or("会议不存在")?;
    let summary = store.latest_summary(meeting_id).map_err(err)?;
    let summary_text = summary.ok_or("该会议尚未生成纪要，请先进行总结")?;

    let mut md = format!("# {} - 会议纪要\n\n", meeting.title);
    md.push_str(&format!("- 时间：{}\n", meeting.started_at));
    md.push_str(&format!(
        "- 时长：{}\n\n",
        format_ts(meeting.duration_ms as u32)
    ));
    md.push_str(&summary_text);
    md.push_str("\n");

    let p = PathBuf::from(&target_path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    std::fs::write(&p, md).map_err(err)?;
    Ok(p.display().to_string())
}

#[tauri::command]
fn get_meeting_share_text(state: State<AppState>, meeting_id: i64) -> R<String> {
    let store = state.store()?;
    let meeting = store
        .get_meeting(meeting_id)
        .map_err(err)?
        .ok_or("会议不存在")?;
    let summary = store.latest_summary(meeting_id).map_err(err)?;

    if let Some(s) = summary {
        Ok(format!(
            "# {}\n时间：{} ｜ 时长：{}\n\n{}",
            meeting.title,
            meeting.started_at,
            format_ts(meeting.duration_ms as u32),
            s
        ))
    } else {
        let utterances = store.load_utterances(meeting_id).map_err(err)?;
        let mut text = format!(
            "# {}\n时间：{} ｜ 时长：{}\n（暂无总结纪要）\n\n## 逐字稿节选\n",
            meeting.title,
            meeting.started_at,
            format_ts(meeting.duration_ms as u32)
        );
        for u in utterances.iter().take(10) {
            text.push_str(&format!("{}: {}\n", u.display_speaker(), u.text));
        }
        if utterances.len() > 10 {
            text.push_str(&format!("... 共 {} 条发言\n", utterances.len()));
        }
        Ok(text)
    }
}

#[tauri::command]
fn get_config(state: State<AppState>) -> R<Config> {
    state.config()
}

#[tauri::command]
fn save_config(state: State<AppState>, config: Config) -> R<()> {
    config.ensure_dirs().map_err(err)?;
    config.save(&state.config_path).map_err(err)?;
    *state.config.lock().map_err(|_| "配置锁中毒")? = config;
    Ok(())
}

#[tauri::command]
fn set_api_key(key: String) -> R<()> {
    store::set_api_key(&key).map_err(err)
}

#[tauri::command]
fn has_api_key() -> bool {
    store::get_api_key().is_some()
}

#[tauri::command]
async fn test_llm_connection(state: State<'_, AppState>) -> R<String> {
    let cfg = state.config()?;
    let client = LlmClient::new(cfg.llm.clone(), cfg.egress_policy, store::get_api_key())
        .map_err(err)?;
    client.test_connection().await.map_err(err)
}

#[tauri::command]
fn audit_count(state: State<AppState>) -> R<i64> {
    state.store()?.egress_count().map_err(err)
}

/// 供 UI 在切换到「允许任意端点」前，明确告知风险。
#[tauri::command]
fn egress_policy_labels() -> Vec<(String, String)> {
    vec![
        (
            EgressPolicy::LocalOnly.as_str().to_string(),
            "仅本机端点（推荐）— 逐字稿不会离开这台电脑".to_string(),
        ),
        (
            EgressPolicy::Open.as_str().to_string(),
            "允许任意端点 — 逐字稿将发送到你配置的服务商".to_string(),
        ),
    ]
}

// ---------------------------------------------------------------- 菜单栏托盘

const TRAY_ID: &str = "vocmeet-status";
const TRAY_IDLE_PNG: &[u8] = include_bytes!("../icons/tray-idle.png");
const TRAY_RECORDING_PNG: &[u8] = include_bytes!("../icons/tray-recording.png");

/// 第一次点关闭时提醒「不会直接退出」。记在数据目录里，不进配置文件——
/// 这只是一次提示，不该出现在用户能改的设置里。
#[cfg(windows)]
fn close_hint_flag_path() -> PathBuf {
    vocmeet_core::config::default_config_path()
        .parent()
        .map(|d| d.join("close-hint.seen"))
        .unwrap_or_else(|| PathBuf::from("close-hint.seen"))
}

/// 无边框窗口上，Tauri 自己记的「已最大化」会和系统实际状态错开：
/// 双击标题栏最大化之后，再双击它仍认为没最大化，于是又最大化一次，还原不了。
/// 这里按系统的 `IsZoomed` 判断，已经铺满就还原。
#[cfg(windows)]
#[tauri::command]
fn toggle_maximize(window: tauri::WebviewWindow) {
    let zoomed = window
        .hwnd()
        .ok()
        .map(|hwnd| {
            use windows::Win32::Foundation::HWND;
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsZoomed(HWND(hwnd.0)).as_bool() }
        })
        .unwrap_or(false);
    if zoomed {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

/// 点关闭：收到托盘，不退出。
///
/// 第一次先不收，让界面在关闭按钮旁把这件事说清楚；
/// 再点一次才真正收起。更新安装走 `app.exit`，不经过这里。
#[cfg(windows)]
fn hide_to_tray(window: &tauri::Window) {
    let seen = close_hint_flag_path().is_file();
    if !seen {
        if let Some(dir) = close_hint_flag_path().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(close_hint_flag_path(), b"1");
        let _ = window.emit("close-to-tray-hint", ());
        return;
    }
    let _ = window.hide();
}

/// 菜单栏常驻图标。空闲时是单色 template 图标（自动适配深浅色菜单栏），
/// 录制时换成砖红实心——窗口不在前台也能一眼看到还在录。
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItem::with_id(app, "tray-show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "退出 VocMeet", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tauri::image::Image::from_bytes(TRAY_IDLE_PNG)?)
        .icon_as_template(true)
        .menu(&menu)
        // 左键留给「回到窗口」这个高频动作，菜单走右键。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray-show" => focus_main_window(app),
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                focus_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn focus_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 切换托盘的录制态。失败只记日志——指示器坏了不该影响录制本身。
///
/// 录制态那张图是彩色的，必须关掉 template 模式，
/// 否则 macOS 会把颜色抹掉、只留下轮廓，红色高亮就白做了。
fn set_tray_recording(app: &AppHandle, recording: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let bytes = if recording {
        TRAY_RECORDING_PNG
    } else {
        TRAY_IDLE_PNG
    };
    match tauri::image::Image::from_bytes(bytes) {
        Ok(img) => {
            let _ = tray.set_icon_as_template(!recording);
            if let Err(e) = tray.set_icon(Some(img)) {
                tracing::warn!("切换托盘图标失败: {e}");
            }
            let _ = tray.set_tooltip(Some(if recording {
                "VocMeet 正在录制"
            } else {
                "VocMeet"
            }));
        }
        Err(e) => tracing::warn!("托盘图标解码失败: {e}"),
    }
}

// ---------------------------------------------------------------- 命令：检查更新
//
// 不走 Tauri 官方 updater（要求维护签名密钥）。仓库是 public 的，
// 直接读 GitHub Releases 的公开 API 就够了——下载下来的就是官方 Release
// 附件本身，交给系统自带的安装器去装。
//
// `install_update` 不接受前端传来的 URL/文件名：那样等于给 webview 开了个
// 「下载任意地址并直接执行」的口子。实际装哪个包，只认后端自己在
// `check_update` 里查到、存进 `AppState.pending_update` 的那份。

const UPDATE_REPO: &str = "mblackcat/vocmeet";

#[derive(Serialize)]
struct UpdateCheckPayload {
    available: bool,
    newer_version_exists: bool,
    current_version: String,
    latest_version: String,
    notes: String,
    asset_name: Option<String>,
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn check_update(app: AppHandle, state: State<'_, AppState>) -> R<UpdateCheckPayload> {
    let current = app.package_info().version.to_string();
    let info = vocmeet_core::update::check_latest(UPDATE_REPO, &current)
        .await
        .map_err(err)?;

    let payload = UpdateCheckPayload {
        available: info.available,
        newer_version_exists: info.newer_version_exists,
        current_version: current,
        latest_version: info.latest_version.clone(),
        notes: info.notes.clone(),
        asset_name: info.asset_name.clone(),
    };
    *state.pending_update.lock().map_err(|_| "更新状态锁中毒")? = Some(info);
    Ok(payload)
}

#[derive(Serialize, Clone)]
struct UpdateProgressEvent {
    received: u64,
    total: Option<u64>,
}

#[derive(Serialize, Clone)]
struct UpdateDoneEvent {
    ok: bool,
    message: String,
}

/// 下载地址是否落在 GitHub 自己的域名上——`check_latest` 已经只从
/// GitHub API 响应里取 `browser_download_url`，这里是双保险，不指望它真的拦到什么。
fn is_trusted_release_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host == "github.com" || host.ends_with(".github.com") || host.ends_with("githubusercontent.com")
}

#[tauri::command]
fn install_update(app: AppHandle, state: State<AppState>) -> R<()> {
    if state
        .update_cancel
        .lock()
        .map_err(|_| "更新锁中毒")?
        .is_some()
    {
        return Err("已经在安装了".into());
    }

    let pending = state
        .pending_update
        .lock()
        .map_err(|_| "更新状态锁中毒")?
        .clone();
    let pending = pending.ok_or("请先点「检查更新」")?;
    if !pending.available {
        return Err("没有可安装的更新".into());
    }
    let asset_url = pending.asset_url.ok_or("没有可安装的更新")?;
    let asset_name = pending.asset_name.ok_or("没有可安装的更新")?;

    if !is_trusted_release_host(&asset_url) {
        return Err("更新地址不受信任，已拒绝".into());
    }
    // 只取文件名部分，防止路径穿越（`../../` 之类）逃出临时目录。
    let safe_name = std::path::Path::new(&asset_name)
        .file_name()
        .ok_or("非法的更新文件名")?
        .to_owned();
    let dst = std::env::temp_dir().join(safe_name);

    let cancel = CancelToken::new();
    *state.update_cancel.lock().map_err(|_| "更新锁中毒")? = Some(cancel.clone());

    std::thread::spawn(move || {
        let progress_app = app.clone();
        let outcome = tokio::runtime::Runtime::new()
            .map_err(|e| e.to_string())
            .and_then(|rt| {
                rt.block_on(vocmeet_core::update::download_update(
                    &asset_url,
                    &dst,
                    |received, total| {
                        let _ = progress_app.emit(
                            "update-download-progress",
                            UpdateProgressEvent { received, total },
                        );
                    },
                    &|| cancel.is_cancelled(),
                ))
                .map_err(|e| e.to_string())
            })
            .and_then(|()| launch_installer(&dst));

        let message = match &outcome {
            Ok(()) => "安装程序已启动，应用即将关闭以完成更新…".to_string(),
            Err(e) => e.clone(),
        };
        let _ = app.emit(
            "update-install-done",
            UpdateDoneEvent {
                ok: outcome.is_ok(),
                message,
            },
        );

        if let Some(state) = app.try_state::<AppState>() {
            if let Ok(mut g) = state.update_cancel.lock() {
                *g = None;
            }
        }

        if outcome.is_ok() {
            // 给安装器一点时间把自己跑起来，再退出好释放当前 exe 的文件锁。
            std::thread::sleep(std::time::Duration::from_millis(500));
            app.exit(0);
        }
    });

    Ok(())
}

#[tauri::command]
fn cancel_update_install(state: State<AppState>) -> R<()> {
    if let Some(c) = state
        .update_cancel
        .lock()
        .map_err(|_| "更新锁中毒")?
        .as_ref()
    {
        c.cancel();
    }
    Ok(())
}

fn launch_installer(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "msi" {
            std::process::Command::new("msiexec")
                .arg("/i")
                .arg(path)
                .spawn()
                .map_err(err)?;
        } else {
            std::process::Command::new(path).spawn().map_err(err)?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map_err(err)?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(err)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 入口

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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // 配置固定放数据目录。原先是 `PathBuf::from("vocmeet.config.json")`——
    // 相对路径跟着工作目录跑，装到 Program Files 之后既读不到也写不进去，
    // 于是每次都回落到默认值，`models_dir` 也就永远是那个解析不了的 "models"。
    let config_path = vocmeet_core::config::default_config_path();
    let mut config = Config::load(&config_path).unwrap_or_default();

    // 权重目录：认可执行文件旁边和工作目录下已有的那一套，
    // 省得开发机上明明下好了却还要去设置页点一遍。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    config.adopt_existing_models(exe_dir.as_deref());
    let _ = config.ensure_dirs();

    let master_key = match store::master_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("无法从 Keychain 取得数据库主密钥：{e}");
            eprintln!("应用无法在不加密的情况下运行，退出。");
            std::process::exit(1);
        }
    };

    let audio_dir = config.audio_dir();
    let templates_dir = config.templates_dir.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            // 只改 Windows。Mac 的关闭仍走系统红绿灯。
            #[cfg(windows)]
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                hide_to_tray(window);
            }
            #[cfg(not(windows))]
            let _ = (window, event);
        })
        .setup(move |app| {
            // titleBarStyle 只在 macOS 去掉标题栏。Windows 得自己摘掉装饰，
            // 否则系统标题「VocMeet」会和侧栏里的标题叠在一起。
            // 运行时摘，而不是写进配置：配置是全平台的，Mac 还要留着红绿灯。
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_decorations(false);
                let _ = window.set_shadow(true);
            }
            // 提示词模板随安装包走。原来指的是相对路径 "templates"，
            // 装完之后同样解析不到——纪要会在「读模板」这一步失败。
            if !templates_dir.join("meeting_default.yaml").is_file() {
                if let Ok(res) = app.path().resource_dir() {
                    let bundled = res.join("templates");
                    if bundled.join("meeting_default.yaml").is_file() {
                        if let Some(state) = app.try_state::<AppState>() {
                            if let Ok(mut cfg) = state.config.lock() {
                                cfg.templates_dir = bundled;
                                let _ = cfg.save(&state.config_path);
                            }
                        }
                    }
                }
            }
            // 回放音轨要能被 webview 直接播放。范围只放开音频目录，
            // 不用通配符——webview 能读什么文件是安全边界的一部分。
            if let Err(e) = app.asset_protocol_scope().allow_directory(&audio_dir, true) {
                tracing::warn!("无法为音频目录开放 asset 协议: {e}");
            }
            if let Err(e) = build_tray(app.handle()) {
                // 托盘只是状态指示，建不起来不该拦住应用启动。
                tracing::warn!("菜单栏图标初始化失败: {e}");
            }
            Ok(())
        })
        .manage(AppState {
            config: Mutex::new(config),
            config_path,
            master_key,
            recording: Mutex::new(None),
            transcribe_cancel: Mutex::new(None),
            last_chunks: Arc::new(Mutex::new(None)),
            model_fetch_cancel: Mutex::new(None),
            llm_pulling: Mutex::new(false),
            pending_update: Mutex::new(None),
            update_cancel: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            doctor,
            list_devices,
            list_meetings,
            list_meetings_page,
            get_meeting_detail,
            process_meeting,
            import_audio,
            importable_extensions,
            reprocess_meeting,
            meeting_has_audio,
            model_download_plan,
            download_models,
            cancel_model_download,
            set_models_dir,
            set_transcripts_dir,
            list_all_participants,
            diagnose_llm,
            pull_llm_model,
            suggested_llm_models,
            get_meeting,
            delete_meeting,
            archive_meeting,
            list_archived_meetings,
            start_recording,
            stop_recording,
            recording_status,
            transcribe,
            cancel_transcribe,
            get_transcript,
            update_utterance,
            list_speakers,
            name_speaker,
            get_note,
            save_note,
            summarize,
            get_summary,
            export_markdown,
            export_meeting_audio,
            export_transcript_markdown,
            export_summary_markdown,
            get_meeting_share_text,
            get_config,
            save_config,
            set_api_key,
            has_api_key,
            test_llm_connection,
            audit_count,
            egress_policy_labels,
            app_version,
            check_update,
            install_update,
            cancel_update_install,
            #[cfg(windows)]
            toggle_maximize,
        ])
        .run(tauri::generate_context!())
        .expect("启动 VocMeet 失败");
}
