#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! VocMeet 桌面应用。UI 通过 Tauri command 调用 core，长任务用 event 回推进度。
//!
//! 设计约束：
//! - 每个命令自己开 Store（SQLite 打开很便宜，WAL 支持并发），避免把
//!   非 Sync 的 `Connection` 塞进全局 state；
//! - 转写与纪要生成在独立线程/运行时里跑，UI 线程永不阻塞；
//! - 录制状态（StopSignal）是唯一需要跨命令共享的可变状态。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

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

    let (models_ok, models_message, model_files, total_mb) = match models.verify_present() {
        Ok(()) => {
            let files = models
                .checksums()
                .map_err(err)?
                .into_iter()
                .map(|(name, digest, size)| ModelFile {
                    name,
                    size_mb: size as f64 / 1e6,
                    sha256_prefix: digest[..16].to_string(),
                })
                .collect::<Vec<_>>();
            let total = models.total_size().unwrap_or(0) as f64 / 1e6;
            (true, "全部权重就位".to_string(), files, total)
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
        Ok(()) => (true, "系统回环可用".to_string()),
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

#[tauri::command]
fn start_recording(state: State<AppState>, title: String) -> R<i64> {
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
    let capture_cfg = vocmeet_capture::CaptureConfig {
        out_dir,
        chunk_seconds: cfg.capture.chunk_seconds,
        min_free_bytes: cfg.capture.min_free_bytes,
        record_mic: cfg.capture.record_mic,
        record_system: cfg.capture.record_system,
    };

    let stop = vocmeet_capture::StopSignal::new();
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        vocmeet_capture::record_dual_track(&capture_cfg, &stop_thread)
    });

    *state.recording.lock().map_err(|_| "录制锁中毒")? = Some(RecordingSession {
        meeting_id,
        stop,
        handle,
    });

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
fn stop_recording(state: State<AppState>) -> R<StopResult> {
    let session = state
        .recording
        .lock()
        .map_err(|_| "录制锁中毒")?
        .take()
        .ok_or("当前没有正在进行的录制")?;

    session.stop.stop();
    let rec = session
        .handle
        .join()
        .map_err(|_| "录制线程 panic".to_string())?
        .map_err(err)?;

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

    let result = StopResult {
        meeting_id: session.meeting_id,
        mic_chunks: rec.mic_chunks.len(),
        system_chunks: rec.system_chunks.len(),
        warnings: rec.warnings.clone(),
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
                format!("{model} 已就位，写纪要的模型已切到它")
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

    let mut attendees: Vec<String> = utterances
        .iter()
        .map(|u| u.display_speaker().to_string())
        .collect();
    attendees.sort();
    attendees.dedup();

    let client = LlmClient::new(cfg.llm.clone(), cfg.egress_policy, store::get_api_key())
        .map_err(err)?;

    let input = RenderInput {
        meeting_title: &meeting.title,
        meeting_time: &meeting.started_at,
        attendees: &attendees,
        scratchpad: &scratchpad,
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
        .setup(move |app| {
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
            diagnose_llm,
            pull_llm_model,
            suggested_llm_models,
            get_meeting,
            delete_meeting,
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
            get_config,
            save_config,
            set_api_key,
            has_api_key,
            test_llm_connection,
            audit_count,
            egress_policy_labels,
        ])
        .run(tauri::generate_context!())
        .expect("启动 VocMeet 失败");
}
