export type Source = "mic" | "system";

export interface Utterance {
  id: number;
  speaker_id: string;
  speaker_name: string | null;
  start_ms: number;
  end_ms: number;
  text: string;
  source: Source;
  low_confidence: boolean;
}

export interface Meeting {
  id: number;
  title: string;
  started_at: string;
  ended_at: string | null;
  duration_ms: number;
  status: string;
  archived_at?: string | null;
}

export interface SpeakerRow {
  key: string;
  display_name: string | null;
  sample: string;
  utterance_count: number;
}

export interface DeviceInfo {
  name: string;
  direction: string;
  is_default: boolean;
}

export interface ModelFile {
  name: string;
  size_mb: number;
  sha256_prefix: string;
}

export interface DoctorReport {
  data_dir: string;
  models_dir: string;
  egress_policy: string;
  llm_endpoint: string;
  llm_model: string;
  models_ok: boolean;
  models_message: string;
  model_files: ModelFile[];
  total_model_mb: number;
  db_ok: boolean;
  db_message: string;
  loopback_ok: boolean;
  loopback_message: string;
}

export interface MeetingPage {
  items: Meeting[];
  total: number;
  offset: number;
}

export interface MeetingDetail {
  meeting: Meeting;
  keywords: string[];
  attendees: string[];
  summary: string | null;
  playback_path: string | null;
  transcript_path?: string | null;
  utterance_count: number;
  note: string;
}

export interface ProcessEvent {
  meeting_id: number;
  /** transcribing | summarizing | done | failed */
  phase: string;
  progress: number;
  detail: string;
}

export interface StopResult {
  meeting_id: number;
  mic_chunks: number;
  system_chunks: number;
  warnings: string[];
  duration_ms: number;
}

export interface ProgressEvent {
  meeting_id: number;
  stage: string;
  progress: number;
  detail: string;
}

export interface DoneEvent {
  meeting_id: number;
  ok: boolean;
  message: string;
  utterance_count: number;
}

export interface DeltaEvent {
  meeting_id: number;
  delta: string;
}

export interface LlmConfig {
  api_base: string;
  model: string;
  context_tokens: number;
  temperature: number;
  max_tokens: number;
  connect_timeout_secs: number;
  request_timeout_secs: number;
}

export interface EngineConfig {
  num_threads: number;
  provider: string;
  cluster_threshold: number;
  merge_gap_ms: number;
  max_speech_duration: number;
}

export interface CaptureConfig {
  chunk_seconds: number;
  min_free_bytes: number;
  record_mic: boolean;
  record_system: boolean;
}

export interface AppConfig {
  data_dir: string;
  models_dir: string;
  templates_dir: string;
  transcripts_dir?: string;
  preset_terms?: string[];
  preset_prompt?: string;
  egress_policy: "local_only" | "open";
  llm: LlmConfig;
  engine: EngineConfig;
  capture: CaptureConfig;
}

export interface ParticipantInfo {
  key: string;
  display_name: string | null;
  meeting_count: number;
  utterance_count: number;
  sample: string;
  last_seen: string;
}

export interface ModelPlan {
  models_dir: string;
  total_bytes: number;
  assets: string[];
  already_present: boolean;
}

export interface ModelFetchEvent {
  /** downloading | extracting | verifying | done */
  phase: string;
  index: number;
  total_assets: number;
  label: string;
  received: number;
  total: number | null;
  overall: number;
}

export interface LlmDiagnosis {
  reachable: boolean;
  installed: string[];
  model_present: boolean;
  endpoint: string;
  wanted_model: string;
  message: string;
  hint: string | null;
}

export interface PullEvent {
  /** Ollama 给的阶段文本，如 pulling manifest / verifying sha256 digest。 */
  status: string;
  completed: number | null;
  total: number | null;
}

/** 一个可选模型：[名字, 说明, 上下文长度]。 */
export type SuggestedModel = [string, string, number];

/** 检查更新的结果：直接读 GitHub Releases，不涉及任何签名密钥。 */
export interface UpdateCheckPayload {
  available: boolean;
  current_version: string;
  latest_version: string;
  notes: string;
  asset_url: string | null;
  asset_name: string | null;
}

export interface UpdateProgressEvent {
  received: number;
  total: number | null;
}

/** 字节数变成人看的单位。下载进度里到处要用。 */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1048576).toFixed(0)} MB`;
  return `${(n / 1073741824).toFixed(1)} GB`;
}

export function formatTs(ms: number): string {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(h)}:${pad(m)}:${pad(s)}`;
}
