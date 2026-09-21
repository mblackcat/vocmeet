import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AppConfig,
  MeetingDetail,
  MeetingPage,
  ProcessEvent,
  DeltaEvent,
  DeviceInfo,
  DoctorReport,
  DoneEvent,
  LlmDiagnosis,
  Meeting,
  ModelFetchEvent,
  ModelPlan,
  ProgressEvent,
  PullEvent,
  SuggestedModel,
  SpeakerRow,
  StopResult,
  Utterance,
  ParticipantInfo,
  UpdateCheckPayload,
  UpdateProgressEvent,
} from "./types";

export const api = {
  doctor: () => invoke<DoctorReport>("doctor"),
  listDevices: () => invoke<DeviceInfo[]>("list_devices"),

  listMeetings: () => invoke<Meeting[]>("list_meetings"),
  listMeetingsPage: (offset: number, limit: number) =>
    invoke<MeetingPage>("list_meetings_page", { offset, limit }),
  getMeetingDetail: (meetingId: number) =>
    invoke<MeetingDetail>("get_meeting_detail", { meetingId }),
  processMeeting: (meetingId: number, speakers?: number | null) =>
    invoke<void>("process_meeting", { meetingId, speakers: speakers ?? null }),
  /** 导入一个音频文件，返回新建的会议 id。解码与整理都在后台跑，进度走 process-* 事件。 */
  importAudio: (args: { path: string; title?: string | null; speakers?: number | null }) =>
    invoke<number>("import_audio", {
      path: args.path,
      title: args.title ?? null,
      speakers: args.speakers ?? null,
    }),
  /** 能导入的扩展名。清单在后端，前端不另存一份。 */
  importableExtensions: () => invoke<string[]>("importable_extensions"),
  /** 这场会议的录音还在不在盘上——决定要不要给「重新解析」这个按钮。 */
  meetingHasAudio: (meetingId: number) =>
    invoke<boolean>("meeting_has_audio", { meetingId }),
  /** 重新整理。full = 从录音重跑识别与纪要；summary = 只重写纪要。 */
  reprocessMeeting: (meetingId: number, mode: "full" | "summary", speakers?: number | null) =>
    invoke<void>("reprocess_meeting", { meetingId, mode, speakers: speakers ?? null }),
  getMeeting: (meetingId: number) => invoke<Meeting | null>("get_meeting", { meetingId }),
  deleteMeeting: (meetingId: number) => invoke<void>("delete_meeting", { meetingId }),
  archiveMeeting: (meetingId: number, archived: boolean) =>
    invoke<void>("archive_meeting", { meetingId, archived }),
  listArchivedMeetings: () => invoke<Meeting[]>("list_archived_meetings"),
  exportMeetingAudio: (meetingId: number, targetPath: string) =>
    invoke<string>("export_meeting_audio", { meetingId, targetPath }),
  exportTranscriptMarkdown: (meetingId: number, targetPath: string) =>
    invoke<string>("export_transcript_markdown", { meetingId, targetPath }),
  exportSummaryMarkdown: (meetingId: number, targetPath: string) =>
    invoke<string>("export_summary_markdown", { meetingId, targetPath }),
  getMeetingShareText: (meetingId: number) =>
    invoke<string>("get_meeting_share_text", { meetingId }),

  startRecording: (title: string) => invoke<number>("start_recording", { title }),
  stopRecording: () => invoke<StopResult>("stop_recording"),
  recordingStatus: () => invoke<number | null>("recording_status"),

  transcribe: (args: {
    meetingId: number;
    speakers?: number | null;
    micWav?: string | null;
    systemWav?: string | null;
  }) =>
    invoke<void>("transcribe", {
      meetingId: args.meetingId,
      speakers: args.speakers ?? null,
      micWav: args.micWav ?? null,
      systemWav: args.systemWav ?? null,
    }),
  cancelTranscribe: () => invoke<void>("cancel_transcribe"),

  getTranscript: (meetingId: number) => invoke<Utterance[]>("get_transcript", { meetingId }),
  updateUtterance: (meetingId: number, ordinal: number, text: string) =>
    invoke<void>("update_utterance", { meetingId, ordinal, text }),

  listSpeakers: (meetingId: number) => invoke<SpeakerRow[]>("list_speakers", { meetingId }),
  nameSpeaker: (meetingId: number, speakerKey: string, displayName: string) =>
    invoke<void>("name_speaker", { meetingId, speakerKey, displayName }),

  getNote: (meetingId: number) => invoke<string>("get_note", { meetingId }),
  saveNote: (meetingId: number, content: string) =>
    invoke<void>("save_note", { meetingId, content }),

  summarize: (meetingId: number) => invoke<void>("summarize", { meetingId }),
  getSummary: (meetingId: number) => invoke<string | null>("get_summary", { meetingId }),

  exportMarkdown: (meetingId: number, path: string) =>
    invoke<string>("export_markdown", { meetingId, path }),

  modelDownloadPlan: () => invoke<ModelPlan>("model_download_plan"),
  downloadModels: () => invoke<void>("download_models"),
  cancelModelDownload: () => invoke<void>("cancel_model_download"),
  setModelsDir: (path: string) => invoke<string>("set_models_dir", { path }),
  setTranscriptsDir: (path: string) => invoke<string>("set_transcripts_dir", { path }),
  listAllParticipants: () => invoke<ParticipantInfo[]>("list_all_participants"),
  diagnoseLlm: () => invoke<LlmDiagnosis>("diagnose_llm"),
  pullLlmModel: (model: string) => invoke<void>("pull_llm_model", { model }),
  suggestedLlmModels: () => invoke<SuggestedModel[]>("suggested_llm_models"),

  getConfig: () => invoke<AppConfig>("get_config"),
  saveConfig: (config: AppConfig) => invoke<void>("save_config", { config }),
  setApiKey: (key: string) => invoke<void>("set_api_key", { key }),
  hasApiKey: () => invoke<boolean>("has_api_key"),
  testLlmConnection: () => invoke<string>("test_llm_connection"),
  auditCount: () => invoke<number>("audit_count"),
  egressPolicyLabels: () => invoke<[string, string][]>("egress_policy_labels"),

  appVersion: () => invoke<string>("app_version"),
  /** 直接读 GitHub Releases 判断有没有新版本，全程无密钥、无签名校验。 */
  checkUpdate: () => invoke<UpdateCheckPayload>("check_update"),
  /** 下载 Release 附件并拉起系统自带的安装器（NSIS/MSI/DMG）。 */
  installUpdate: (assetUrl: string, assetName: string) =>
    invoke<void>("install_update", { assetUrl, assetName }),
};

export const events = {
  onProcessProgress: (cb: (e: ProcessEvent) => void): Promise<UnlistenFn> =>
    listen<ProcessEvent>("process-progress", (e) => cb(e.payload)),
  onProcessDone: (cb: (e: ProcessEvent) => void): Promise<UnlistenFn> =>
    listen<ProcessEvent>("process-done", (e) => cb(e.payload)),
  onTranscribeProgress: (cb: (e: ProgressEvent) => void): Promise<UnlistenFn> =>
    listen<ProgressEvent>("transcribe-progress", (e) => cb(e.payload)),
  onTranscribeDone: (cb: (e: DoneEvent) => void): Promise<UnlistenFn> =>
    listen<DoneEvent>("transcribe-done", (e) => cb(e.payload)),
  onSummarizeDelta: (cb: (e: DeltaEvent) => void): Promise<UnlistenFn> =>
    listen<DeltaEvent>("summarize-delta", (e) => cb(e.payload)),
  onSummarizeDone: (cb: (e: DoneEvent) => void): Promise<UnlistenFn> =>
    listen<DoneEvent>("summarize-done", (e) => cb(e.payload)),
  onModelsProgress: (cb: (e: ModelFetchEvent) => void): Promise<UnlistenFn> =>
    listen<ModelFetchEvent>("models-progress", (e) => cb(e.payload)),
  onModelsDone: (cb: (e: DoneEvent) => void): Promise<UnlistenFn> =>
    listen<DoneEvent>("models-done", (e) => cb(e.payload)),
  onPullProgress: (cb: (e: PullEvent) => void): Promise<UnlistenFn> =>
    listen<PullEvent>("llm-pull-progress", (e) => cb(e.payload)),
  onPullDone: (cb: (e: DoneEvent) => void): Promise<UnlistenFn> =>
    listen<DoneEvent>("llm-pull-done", (e) => cb(e.payload)),
  onUpdateProgress: (cb: (e: UpdateProgressEvent) => void): Promise<UnlistenFn> =>
    listen<UpdateProgressEvent>("update-download-progress", (e) => cb(e.payload)),
};

/** Tauri command 的错误是字符串，统一成消息文本方便 UI 处理。 */
export function asMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}
