import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, asMessage, events } from "../api";
import type {
  AppConfig,
  DeviceInfo,
  DoctorReport,
  LlmDiagnosis,
  Meeting,
  ModelFetchEvent,
  ModelPlan,
  PullEvent,
  SuggestedModel,
  UpdateCheckPayload,
  UpdateProgressEvent,
} from "../types";
import { formatBytes, formatTs } from "../types";
import QuickNav from "../components/QuickNav";

interface Props {
  onError: (msg: string) => void;
  onMeetingChanged?: () => void;
}

function phaseText(phase: string): string {
  if (phase === "downloading") return "下载中";
  if (phase === "extracting") return "解压中";
  if (phase === "verifying") return "校验中";
  return "完成";
}

function isLocalEndpoint(url: string): boolean {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.toLowerCase();
    return (
      host === "localhost" ||
      host === "127.0.0.1" ||
      host === "::1" ||
      host.startsWith("127.")
    );
  } catch {
    return url.includes("localhost") || url.includes("127.0.0.1");
  }
}

type ProviderId = "ollama" | "openai" | "anthropic" | "gemini";
/**
 * chat、chat_compat 都走 /chat/completions，差别只在给中转用的说明。
 * responses 走 /responses。
 */
type ApiFormat = "chat" | "chat_compat" | "responses";

interface ProviderPreset {
  id: ProviderId;
  name: string;
  desc: string;
  apiBase: string;
  defaultModel: string;
  candidateModels: string[];
  contextTokens: number;
  requiresKey: boolean;
  keyHint: string;
  egressPolicy: "local_only" | "open";
  /** OpenAI 额外可选接口格式。 */
  formats?: { id: ApiFormat; name: string; desc: string }[];
}

const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    id: "ollama",
    name: "本地 Ollama",
    desc: "离线优先，全流程本地闭环",
    apiBase: "http://localhost:11434/v1",
    defaultModel: "qwen2.5:14b",
    candidateModels: ["qwen2.5:14b", "qwen2.5-coder:14b", "qwen2.5:7b", "glm4:9b"],
    contextTokens: 16384,
    requiresKey: false,
    keyHint: "本地模型通常无需填写密钥",
    egressPolicy: "local_only",
  },
  {
    id: "openai",
    name: "OpenAI",
    desc: "官方接口，也可改成中转地址",
    apiBase: "https://api.openai.com/v1",
    defaultModel: "gpt-5.6-sol",
    candidateModels: ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-6-astra"],
    contextTokens: 65536,
    requiresKey: true,
    keyHint: "填入 OpenAI 或中转网关的 Bearer Token",
    egressPolicy: "open",
    formats: [
      { id: "chat", name: "Chat 格式", desc: "官方 /chat/completions" },
      { id: "chat_compat", name: "Chat 兼容格式", desc: "同样走 /chat/completions，给只实现了 Chat 的中转" },
      { id: "responses", name: "Responses 格式", desc: "官方 /responses" },
    ],
  },
  {
    id: "anthropic",
    name: "Anthropic",
    desc: "Claude，走 OpenAI 兼容接口",
    apiBase: "https://api.anthropic.com/v1",
    defaultModel: "claude-opus-5",
    candidateModels: ["claude-opus-5", "claude-sonnet-5", "claude-opus-4-8", "claude-fable-5"],
    contextTokens: 65536,
    requiresKey: true,
    keyHint: "在 Anthropic Console 获取 API Key",
    egressPolicy: "open",
  },
  {
    id: "gemini",
    name: "Google Gemini",
    desc: "Gemini 原生接口，地址一般到 /v1beta",
    apiBase: "https://generativelanguage.googleapis.com/v1beta",
    defaultModel: "gemini-3.8-flash",
    candidateModels: ["gemini-3.8-flash"],
    contextTokens: 65536,
    requiresKey: true,
    keyHint: "请在 Google AI Studio 获取 API Key (AIzaSy...)",
    egressPolicy: "open",
  },
];

/** 旧配置没有 provider 字段，按地址猜一次。猜不到就当地址是手改过的，不选中任何一家。 */
function inferProvider(cfg: AppConfig): ProviderId | null {
  const saved = cfg.llm.provider;
  if (saved === "ollama" || saved === "openai" || saved === "anthropic" || saved === "gemini") {
    return saved;
  }
  const base = cfg.llm.api_base;
  if (isLocalEndpoint(base)) return "ollama";
  if (base.includes("anthropic.com")) return "anthropic";
  if (base.includes("googleapis.com")) return "gemini";
  if (base.includes("openai.com")) return "openai";
  return null;
}

const GAME_INDUSTRY_TERMS = [
  "ASR", "NPC", "PVP", "PVE", "DAU", "MAU", "MMORPG", "GaaS", "UE5", "Unity",
  "骨骼动画", "帧同步", "状态同步", "数值平衡", "次留", "ARPU", "物理引擎", "光追"
];

export default function Settings({ onError, onMeetingChanged }: Props) {
  const [cfg, setCfg] = useState<AppConfig | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [hasKey, setHasKey] = useState(false);
  const [audits, setAudits] = useState(0);
  const [msg, setMsg] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  const [report, setReport] = useState<DoctorReport | null>(null);
  const [devices, setDevices] = useState<DeviceInfo[]>([]);
  const [plan, setPlan] = useState<ModelPlan | null>(null);
  const [fetching, setFetching] = useState<ModelFetchEvent | null>(null);
  const [llm, setLlm] = useState<LlmDiagnosis | null>(null);
  const [probing, setProbing] = useState(false);
  const [suggested, setSuggested] = useState<SuggestedModel[]>([]);
  const [pulling, setPulling] = useState<PullEvent | null>(null);
  const [archivedList, setArchivedList] = useState<Meeting[]>([]);
  const [newTerm, setNewTerm] = useState("");

  const [appVersion, setAppVersion] = useState("");
  const [updateCheck, setUpdateCheck] = useState<UpdateCheckPayload | null>(null);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [installingUpdate, setInstallingUpdate] = useState(false);
  const [updateProgress, setUpdateProgress] = useState<UpdateProgressEvent | null>(null);

  const loadArchived = async () => {
    try {
      const list = await api.listArchivedMeetings();
      setArchivedList(list);
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const handleRestore = async (id: number, title: string) => {
    try {
      await api.archiveMeeting(id, false);
      setMsg(`已恢复会议「${title}」至主列表`);
      await loadArchived();
      onMeetingChanged?.();
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const handleDelete = async (id: number, title: string) => {
    if (
      !window.confirm(
        `确定要彻底删除会议「${title}」吗？\n所有录音、逐字稿与纪要将被永久清除且不可恢复。`
      )
    ) {
      return;
    }
    try {
      await api.deleteMeeting(id);
      setMsg(`已永久删除会议「${title}」`);
      await loadArchived();
      onMeetingChanged?.();
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const refreshDoctor = async () => {
    try {
      const [r, p] = await Promise.all([api.doctor(), api.modelDownloadPlan()]);
      setReport(r);
      setPlan(p);
    } catch (e) {
      onError(asMessage(e));
    }
  };

  useEffect(() => {
    void api
      .appVersion()
      .then(setAppVersion)
      .catch((e) => onError(asMessage(e)));
  }, [onError]);

  useEffect(() => {
    const off: Array<() => void> = [];
    void events.onModelsProgress(setFetching).then((f) => off.push(f));
    void events.onPullProgress(setPulling).then((f) => off.push(f));
    void events.onUpdateProgress(setUpdateProgress).then((f) => off.push(f));
    void events
      .onUpdateInstallDone((e) => {
        setInstallingUpdate(false);
        setUpdateProgress(null);
        setMsg(e.message);
        if (!e.ok) onError(e.message);
      })
      .then((f) => off.push(f));
    void events
      .onPullDone((e) => {
        setPulling(null);
        setMsg(e.message);
        if (!e.ok) onError(e.message);
        void api.getConfig().then(setCfg);
        void api.diagnoseLlm().then(setLlm).catch(() => {});
      })
      .then((f) => off.push(f));
    void events
      .onModelsDone((e) => {
        setFetching(null);
        setMsg(e.message);
        if (!e.ok) onError(e.message);
        void refreshDoctor();
        void api.getConfig().then(setCfg);
      })
      .then((f) => off.push(f));
    return () => off.forEach((f) => f());
  }, [onError]);

  useEffect(() => {
    void (async () => {
      try {
        const [c, k, a] = await Promise.all([
          api.getConfig(),
          api.hasApiKey(),
          api.auditCount(),
        ]);
        setCfg(c);
        setHasKey(k);
        setAudits(a);
      } catch (e) {
        onError(asMessage(e));
      }
      try {
        const [r, d, mp, sug] = await Promise.all([
          api.doctor(),
          api.listDevices().catch(() => [] as DeviceInfo[]),
          api.modelDownloadPlan(),
          api.suggestedLlmModels(),
        ]);
        setReport(r);
        setDevices(d);
        setPlan(mp);
        setSuggested(sug);
      } catch (e) {
        onError(asMessage(e));
      }
      void loadArchived();
      // 端点体检要连网，不挡设置页先出来。
      void api.diagnoseLlm().then(setLlm).catch(() => {});
    })();
  }, [onError]);

  if (!cfg) return <div className="hollow">读取中...</div>;

  const patch = (p: Partial<AppConfig>) => setCfg({ ...cfg, ...p });

  const persist = async (next: AppConfig) => {
    const llmChanged =
      JSON.stringify(next.llm) !== JSON.stringify(cfg.llm) ||
      next.egress_policy !== cfg.egress_policy;
    setCfg(next);
    try {
      await api.saveConfig(next);
      setMsg("已保存");
      if (llmChanged) {
        void api.diagnoseLlm().then(setLlm).catch(() => {});
      }
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const handleCheckUpdate = async () => {
    setCheckingUpdate(true);
    setMsg(null);
    try {
      const u = await api.checkUpdate();
      setAppVersion(u.current_version);
      setUpdateCheck(u);
      if (u.available) {
        setMsg(`发现新版本 v${u.latest_version}，点击「安装更新」下载安装`);
      } else if (u.newer_version_exists) {
        setMsg(`发现新版本 v${u.latest_version}，但没有找到适配当前系统的安装包，请前往 GitHub Releases 手动下载`);
      } else {
        setMsg("已经是最新版");
      }
    } catch (e) {
      onError(asMessage(e));
    } finally {
      setCheckingUpdate(false);
    }
  };

  const handleInstallUpdate = async () => {
    if (!updateCheck?.available) return;
    setInstallingUpdate(true);
    setUpdateProgress({ received: 0, total: null });
    setMsg(null);
    try {
      // install_update 立刻返回：下载/安装在后台线程跑，结果走 update-install-done 事件。
      await api.installUpdate();
    } catch (e) {
      onError(asMessage(e));
      setInstallingUpdate(false);
      setUpdateProgress(null);
    }
  };

  const handleCancelInstallUpdate = () => {
    void api.cancelUpdateInstall().catch((e) => onError(asMessage(e)));
  };

  const test = async () => {
    setTesting(true);
    setMsg(null);
    try {
      if (apiKey.trim()) {
        await api.setApiKey(apiKey.trim());
        setApiKey("");
        setHasKey(true);
      }
      const outcome = await api.testLlmConnection();
      setMsg(outcome);
      void api.diagnoseLlm().then(setLlm).catch(() => {});
    } catch (e) {
      onError(asMessage(e));
    } finally {
      setTesting(false);
    }
  };

  const probe = async () => {
    setProbing(true);
    try {
      setLlm(await api.diagnoseLlm());
    } catch (e) {
      onError(asMessage(e));
    } finally {
      setProbing(false);
    }
  };

  const pickModelsDir = async () => {
    try {
      const dir = await open({ directory: true, multiple: false });
      if (typeof dir !== "string") return;
      setMsg(await api.setModelsDir(dir));
      await refreshDoctor();
      setCfg(await api.getConfig());
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const pickTranscriptsDir = async () => {
    try {
      const dir = await open({ directory: true, multiple: false });
      if (typeof dir !== "string") return;
      const res = await api.setTranscriptsDir(dir);
      setMsg(res);
      setCfg(await api.getConfig());
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const storeKey = async () => {
    if (!apiKey.trim()) return;
    try {
      await api.setApiKey(apiKey.trim());
      setApiKey("");
      setHasKey(true);
      setMsg("密钥已保存到系统凭据管理器");
      void api.diagnoseLlm().then(setLlm).catch(() => {});
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const applyPreset = async (p: ProviderPreset) => {
    const next: AppConfig = {
      ...cfg,
      egress_policy: p.egressPolicy,
      llm: {
        ...cfg.llm,
        provider: p.id,
        api_format: p.id === "openai" ? cfg.llm.api_format || "chat" : p.id === "gemini" ? "gemini" : "chat",
        api_base: p.apiBase,
        model: p.defaultModel,
        context_tokens: p.contextTokens,
      },
    };
    await persist(next);
    setMsg(`已切换为「${p.name}」。服务地址和模型都可以再改，不会取消选中。`);
  };

  const setApiFormat = (format: ApiFormat) => {
    const next: AppConfig = { ...cfg, llm: { ...cfg.llm, api_format: format } };
    void persist(next);
  };

  const addTerm = (term: string) => {
    const t = term.trim();
    if (!t) return;
    const existing = cfg.preset_terms || [];
    if (existing.includes(t)) return;
    const next = { ...cfg, preset_terms: [...existing, t] };
    void persist(next);
    setNewTerm("");
  };

  const removeTerm = (term: string) => {
    const existing = cfg.preset_terms || [];
    const next = { ...cfg, preset_terms: existing.filter((item) => item !== term) };
    void persist(next);
  };

  const loadGamePresetTerms = () => {
    const existing = new Set(cfg.preset_terms || []);
    GAME_INDUSTRY_TERMS.forEach((t) => existing.add(t));
    const next = { ...cfg, preset_terms: Array.from(existing) };
    void persist(next);
    setMsg("已载入游戏行业专用词汇库");
  };

  // 设置页面的快速导航项
  const settingsNavItems = [
    { id: "set-rec-asr", label: "录音与识别" },
    { id: "set-transcripts", label: "逐字稿存档" },
    { id: "set-llm", label: "会议总结LLM" },
    { id: "set-machine", label: "会议本地存档" },
  ];

  return (
    <div className="doc-wrapper">
      <div className="doc">
        <div className="meeting-head" id="set-header">
          <h2>设置</h2>
        </div>

        <div className="check update-row" style={{ marginBottom: 14 }}>
          <span className="d">当前版本 v{appVersion || "…"}</span>
          <span className="update-aside">
            {updateCheck?.available && (
              <span className="note update-tip">
                发现新版本 v{updateCheck.latest_version}
                {updateProgress && updateProgress.total && updateProgress.total > 0 && (
                  <span className="thin-bar update-bar">
                    <span
                      className="fill"
                      style={{
                        width: `${Math.min(100, Math.round((updateProgress.received / updateProgress.total) * 100))}%`,
                      }}
                    />
                  </span>
                )}
              </span>
            )}
            {updateCheck && !updateCheck.available && updateCheck.newer_version_exists && (
              <span className="note update-tip">
                发现新版本 v{updateCheck.latest_version}，但没有适配当前系统的安装包
              </span>
            )}
            {updateCheck?.available ? (
              <span className="update-actions">
                {installingUpdate && (
                  <button type="button" className="act" onClick={handleCancelInstallUpdate}>
                    取消
                  </button>
                )}
                <button
                  type="button"
                  className="act"
                  disabled={installingUpdate}
                  onClick={() => void handleInstallUpdate()}
                >
                  {installingUpdate ? "安装中…" : "安装更新"}
                </button>
              </span>
            ) : (
              <button
                type="button"
                className="act"
                disabled={checkingUpdate}
                onClick={() => void handleCheckUpdate()}
              >
                {checkingUpdate ? "检查中…" : "检查更新"}
              </button>
            )}
          </span>
        </div>

        {msg && <div className="msg">{msg}</div>}

        {/* 1. 录音与识别 */}
        <div id="set-rec-asr" className="section-block">
          <h2 className="section">
            <span className="section-title-tag">录音与识别</span>
          </h2>

          {/* 语音识别模型（挪到第一部分） */}
          <div className="subsection-title">语音识别模型</div>
          <p className="note" style={{ marginTop: 0, marginBottom: 12 }}>
            语音识别模型不随安装包提供，需手动下载安装。首次下载默认存放于应用内的 models 目录。
          </p>

          {report && (
            <>
              <div className="check">
                <span className={report.models_ok ? "st ok" : "st no"}>
                  {report.models_ok ? "就绪" : "缺失"}
                </span>
                <span className="d">
                  识别模型{" "}
                  {report.models_ok
                    ? `· 占用空间 ${report.total_model_mb.toFixed(1)} MB`
                    : report.models_message.split(String.fromCharCode(10))[0]}
                  <div className="note">路径：{report.models_dir}</div>
                </span>
              </div>

              {fetching ? (
                <div className="fetching">
                  <div className="thin-bar" style={{ width: "100%", marginTop: 0 }}>
                    <div className="fill" style={{ width: `${Math.round(fetching.overall * 100)}%` }} />
                  </div>
                  <p className="data">
                    {fetching.label} · {phaseText(fetching.phase)}
                    {fetching.phase === "downloading" &&
                      ` ${formatBytes(fetching.received)}${
                        fetching.total ? ` / ${formatBytes(fetching.total)}` : ""
                      }`}
                    {` · 第 ${fetching.index + 1}/${fetching.total_assets} 项`}
                  </p>
                  <button className="act" onClick={() => void api.cancelModelDownload()}>
                    取消下载
                  </button>
                </div>
              ) : (
                !report.models_ok &&
                plan && (
                  <div className="fetching">
                    <p className="note" style={{ marginTop: 0 }}>
                      会从 GitHub 下载约 {formatBytes(plan.total_bytes)}（包含 VAD 静音切分与声纹特征提取权重），
                      存放至 {plan.models_dir}，下载完成后自动就绪生效。
                    </p>
                    <div className="actions" style={{ marginTop: 12, borderTop: 0, paddingTop: 0 }}>
                      <button className="act" onClick={() => void api.downloadModels()}>
                        一键下载模型
                      </button>
                      <button className="act" onClick={() => void pickModelsDir()}>
                        已有模型，指个目录
                      </button>
                    </div>
                  </div>
                )
              )}
            </>
          )}

          <div className="subsection-title" style={{ marginTop: 24 }}>录音装置</div>
          {report && (
            <div className="check">
              <span className={report.loopback_ok ? "st ok" : "st no"}>
                {report.loopback_ok ? "可用" : "不可用"}
              </span>
              <span className="d">
                系统回环内录（捕捉远端/会议软件发声）
                {report.loopback_ok ? "" : ` · ${report.loopback_message}`}
              </span>
            </div>
          )}
          {devices.map((dev, i) => (
            <div className="check" key={i}>
              <span className="st">{dev.direction}</span>
              <span className="d">
                {dev.name}
                {dev.is_default ? " · 默认" : ""}
              </span>
            </div>
          ))}

          {/* 识别引擎与分片参数 */}
          <div className="subsection-title" style={{ marginTop: 24 }}>参数调优</div>
          <div className="set-row">
            <span className="k">识别线程</span>
            <span className="v">
              <input
                type="number"
                value={cfg.engine.num_threads}
                onChange={(e) =>
                  patch({ engine: { ...cfg.engine, num_threads: Number(e.target.value) } })
                }
                onBlur={() => void persist(cfg)}
              />
              <div className="note">CPU 推理核心数，建议保持为物理核心数的 1/2 至 1 倍</div>
            </span>
          </div>
          <div className="set-row">
            <span className="k">分片时长 (秒)</span>
            <span className="v">
              <input
                type="number"
                value={cfg.capture.chunk_seconds}
                onChange={(e) =>
                  patch({ capture: { ...cfg.capture, chunk_seconds: Number(e.target.value) } })
                }
                onBlur={() => void persist(cfg)}
              />
              <div className="note">每隔指定秒数音频自动落盘一次，断电或异常最多损失一个分片</div>
            </span>
          </div>
          <div className="set-row">
            <span className="k">会中实时转写</span>
            <span className="v">
              <label className="choice" style={{ padding: 0 }}>
                <input
                  type="checkbox"
                  checked={cfg.capture.live_transcribe}
                  onChange={(e) => {
                    const next = {
                      ...cfg,
                      capture: { ...cfg.capture, live_transcribe: e.target.checked },
                    };
                    patch({ capture: next.capture });
                    void persist(next);
                  }}
                />
                <span className="c-t">开会过程中就逐段转成文字</span>
              </label>
            </span>
          </div>
          <div className="set-row">
            <span className="k">实时稿延迟 (秒)</span>
            <span className="v">
              <input
                type="number"
                value={cfg.capture.live_segment_seconds}
                disabled={!cfg.capture.live_transcribe}
                onChange={(e) =>
                  patch({
                    capture: {
                      ...cfg.capture,
                      live_segment_seconds: Number(e.target.value),
                    },
                  })
                }
                onBlur={() => void persist(cfg)}
              />
              <div className="note">
                每攒够这么多秒就转写一次，也就是文字出现的延迟。
              </div>
            </span>
          </div>
        </div>

        {/* 2. 逐字稿存档（落盘目录修改，移除原有仅本机和任意端点单选项） */}
        <div id="set-transcripts" className="section-block">
          <h2 className="section">
            <span className="section-title-tag">逐字稿存档</span>
          </h2>

          <div className="set-row">
            <span className="k">逐字稿落盘目录</span>
            <span className="v">
              <div style={{ display: "flex", gap: 10, alignItems: "center" }}>
                <input
                  type="text"
                  readOnly
                  value={cfg.transcripts_dir || `${cfg.data_dir}\\transcripts`}
                  style={{ flex: 1, color: "var(--ink)" }}
                />
                <button
                  type="button"
                  className="act"
                  style={{ margin: 0, padding: "5px 12px", whiteSpace: "nowrap" }}
                  onClick={() => void pickTranscriptsDir()}
                >
                  修改目录
                </button>
              </div>
              <div className="note">
                会议识别完成后的逐字稿 Markdown 文件将自动输出至此目录。修改落盘目录不影响已有旧文稿（每场会议记录均绑定生成时的绝对地址）。
              </div>
            </span>
          </div>

          <div className="set-row">
            <span className="k">出网审计</span>
            <span className="v">
              <span className="data">已记录 {audits} 次模型调用审计</span>
              <div className="note">
                审计仅记录端点、模型名称和发送字符数，不存储任何会议发言正文。
              </div>
            </span>
          </div>
        </div>

        {/* 3. 会议总结LLM（API密钥挪到模型后、就绪提示前，增加预设信息） */}
        <div id="set-llm" className="section-block">
          <h2 className="section">
            <span className="section-title-tag">会议总结LLM</span>
          </h2>

          {/* 预设服务商按钮 */}
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", margin: "14px 0 16px" }}>
            {PROVIDER_PRESETS.map((p) => {
              const active = inferProvider(cfg) === p.id;
              return (
                <button
                  key={p.id}
                  className={`act ${active ? "on" : ""}`}
                  style={{
                    padding: "6px 14px",
                    background: active ? "var(--paper-2)" : "none",
                    borderColor: active ? "var(--ink)" : "var(--rule)",
                    fontWeight: active ? 400 : 300,
                  }}
                  onClick={() => void applyPreset(p)}
                  title={p.desc}
                >
                  {p.name}
                </button>
              );
            })}
          </div>

          {(() => {
            const current = PROVIDER_PRESETS.find((p) => p.id === inferProvider(cfg));
            if (!current?.formats) return null;
            const selected: ApiFormat =
              cfg.llm.api_format === "responses" || cfg.llm.api_format === "chat_compat"
                ? cfg.llm.api_format
                : "chat";
            return (
              <div className="set-row">
                <span className="k">接口格式</span>
                <span className="v">
                  <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                    {current.formats.map((f) => (
                      <button
                        key={f.id}
                        type="button"
                        className="act"
                        title={f.desc}
                        style={{
                          padding: "5px 12px",
                          background: selected === f.id ? "var(--paper-2)" : "none",
                          borderColor: selected === f.id ? "var(--ink)" : "var(--rule)",
                        }}
                        onClick={() => setApiFormat(f.id)}
                      >
                        {f.name}
                      </button>
                    ))}
                  </div>
                  <div className="note">
                    {current.formats.find((f) => f.id === selected)?.desc}
                    。服务地址可以改成中转，格式保持不变。
                  </div>
                </span>
              </div>
            );
          })()}

          {/* 服务地址 */}
          <div className="set-row">
            <span className="k">服务地址</span>
            <span className="v">
              <input
                type="text"
                value={cfg.llm.api_base}
                placeholder="http://localhost:11434/v1"
                onChange={(e) => patch({ llm: { ...cfg.llm, api_base: e.target.value } })}
                onBlur={() => void persist(cfg)}
              />
              <div className="note">
                {isLocalEndpoint(cfg.llm.api_base)
                  ? "当前为本地端点（Ollama / vLLM / llama.cpp）"
                  : inferProvider(cfg) === "gemini"
                    ? "当前为 Gemini 原生接口，请求发到 /v1beta/models/{模型}:generateContent"
                    : "当前为远端端点（OpenAI 兼容协议）"}
              </div>
            </span>
          </div>

          {/* 模型名称 */}
          <div className="set-row">
            <span className="k">模型</span>
            <span className="v">
              <input
                type="text"
                list="llm-models"
                value={cfg.llm.model}
                onChange={(e) => patch({ llm: { ...cfg.llm, model: e.target.value } })}
                onBlur={() => void persist(cfg)}
              />
              <datalist id="llm-models">
                {(llm?.installed ?? []).map((m) => (
                  <option value={m} key={m} />
                ))}
              </datalist>

              {(() => {
                const currentPreset = PROVIDER_PRESETS.find((p) => p.id === inferProvider(cfg));
                if (!currentPreset || currentPreset.candidateModels.length === 0) return null;
                return (
                  <div style={{ display: "flex", gap: 8, marginTop: 6, flexWrap: "wrap", alignItems: "baseline" }}>
                    <span className="note" style={{ marginTop: 0 }}>快捷填入：</span>
                    {currentPreset.candidateModels.map((m) => (
                      <button
                        key={m}
                        className="link"
                        style={{
                          fontSize: 12,
                          color: cfg.llm.model === m ? "var(--ink)" : "var(--ink-3)",
                          textDecoration: cfg.llm.model === m ? "underline" : "none",
                          fontWeight: cfg.llm.model === m ? 500 : 300,
                          cursor: "pointer",
                          border: 0,
                          background: "none",
                          padding: "1px 4px",
                        }}
                        onClick={() => {
                          patch({ llm: { ...cfg.llm, model: m } });
                          void persist({ ...cfg, llm: { ...cfg.llm, model: m } });
                        }}
                      >
                        {m}
                      </button>
                    ))}
                  </div>
                );
              })()}
            </span>
          </div>

          {/* API密钥（挪到模型字段后，在就绪提示前） */}
          <div className="set-row">
            <span className="k">API 密钥</span>
            <span className="v">
              <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                <input
                  type="password"
                  value={apiKey}
                  placeholder={hasKey ? "已在系统凭据库保存，留空则不修改" : "远端模型填写 API Token (如 AIzaSy... / sk-...)"}
                  onChange={(e) => setApiKey(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && apiKey.trim()) void storeKey();
                  }}
                  style={{ flex: 1 }}
                />
                <button
                  type="button"
                  className="act"
                  disabled={!apiKey.trim()}
                  onClick={() => void storeKey()}
                  style={{ margin: 0, padding: "5px 12px", whiteSpace: "nowrap" }}
                >
                  保存密钥
                </button>
              </div>
              <div className="note">
                {hasKey ? "凭据管理器已存有密钥。" : "尚未存储密钥。"}
                {(() => {
                  const currentPreset = PROVIDER_PRESETS.find((p) => p.id === inferProvider(cfg));
                  return currentPreset
                    ? ` ${currentPreset.keyHint}`
                    : " 远端兼容服务使用 Bearer Token 认证。";
                })()}
              </div>
            </span>
          </div>

          {/* 就绪检查提示卡片（在密钥之后） */}
          {llm && (
            <div className="check" style={{ marginTop: 14 }}>
              <span className={llm.reachable && llm.model_present ? "st ok" : "st no"}>
                {llm.reachable ? (llm.model_present ? "就绪" : "缺模型") : "连不上"}
              </span>
              <span className="d">
                {llm.message}
                {llm.hint && <div className="note">{llm.hint}</div>}
              </span>
            </div>
          )}

          {/* 本地端点拉取模型 */}
          {isLocalEndpoint(cfg.llm.api_base) &&
            (pulling ? (
              <div className="fetching">
                {pulling.total ? (
                  <div className="thin-bar" style={{ width: "100%", marginTop: 0 }}>
                    <div
                      className="fill"
                      style={{
                        width: `${Math.round(((pulling.completed ?? 0) / pulling.total) * 100)}%`,
                      }}
                    />
                  </div>
                ) : null}
                <p className="data">
                  {pulling.status}
                  {pulling.total
                    ? ` · ${formatBytes(pulling.completed ?? 0)} / ${formatBytes(pulling.total)}`
                    : ""}
                </p>
                <p className="note" style={{ marginTop: 0 }}>
                  由 Ollama 自身后台下载，切换界面不会中断。
                </p>
              </div>
            ) : (
              llm?.reachable &&
              !llm.model_present && (
                <div className="fetching">
                  <p className="note" style={{ marginTop: 0 }}>
                    让本地 Ollama 自动拉取模型，完成后切入使用：
                  </p>
                  <div className="actions" style={{ marginTop: 10, borderTop: 0, paddingTop: 0 }}>
                    {suggested.map(([name, desc]) => (
                      <button
                        className="act"
                        key={name}
                        title={desc}
                        onClick={() => void api.pullLlmModel(name)}
                      >
                        拉取 {name}
                      </button>
                    ))}
                  </div>
                </div>
              )
            ))}

          <div className="actions" style={{ marginTop: 16, borderTop: 0, paddingTop: 0 }}>
            <button type="button" className="act" disabled={testing} onClick={() => void test()}>
              {testing ? "连接中..." : "测试连接"}
            </button>
            <button type="button" className="act" disabled={probing} onClick={() => void probe()}>
              {probing ? "检查中..." : "重新检查端点"}
            </button>
          </div>

          <div className="set-row">
            <span className="k">上下文长度</span>
            <span className="v">
              <input
                type="number"
                value={cfg.llm.context_tokens}
                onChange={(e) =>
                  patch({ llm: { ...cfg.llm, context_tokens: Number(e.target.value) } })
                }
                onBlur={() => void persist(cfg)}
              />
              <div className="note">
                长会议超出上限时自动进行分段提要再合成（本地模型建议 16384，云端长文本建议 65536）。
              </div>
            </span>
          </div>

          {/* 新增预设信息模块：行业专用词与系统提示词管理 */}
          <div className="subsection-title" style={{ marginTop: 28 }}>预设信息与专用词库</div>
          <p className="note" style={{ marginTop: 0, marginBottom: 14 }}>
            预先设定行业术语词汇与提示词补充指令，可显著提升语音识别纠错与纪要提炼的专业准确度。
          </p>

          <div className="set-row">
            <span className="k">行业专用词库</span>
            <span className="v">
              <div className="preset-terms-wrap">
                {(cfg.preset_terms || []).map((term) => (
                  <span className="preset-term-chip" key={term}>
                    <span>{term}</span>
                    <button
                      type="button"
                      className="term-del-btn"
                      onClick={() => removeTerm(term)}
                      aria-label={`删除词 ${term}`}
                    >
                      ×
                    </button>
                  </span>
                ))}
              </div>

              <div style={{ display: "flex", gap: 8, marginTop: 10, alignItems: "center" }}>
                <input
                  type="text"
                  value={newTerm}
                  placeholder="输入术语/缩写（如 UE5、DAU、帧同步），回车添加"
                  onChange={(e) => setNewTerm(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") addTerm(newTerm);
                  }}
                  style={{ flex: 1 }}
                />
                <button
                  type="button"
                  className="act"
                  style={{ margin: 0, padding: "5px 12px" }}
                  onClick={() => addTerm(newTerm)}
                >
                  添加词
                </button>
                <button
                  type="button"
                  className="act"
                  style={{ margin: 0, padding: "5px 12px", fontSize: 12 }}
                  title="填入游戏行业常用词（ASR/NPC/PVP/PVE/DAU/MAU/MMO/GaaS/骨骼动画等）"
                  onClick={loadGamePresetTerms}
                >
                  载入游戏行业常用词
                </button>
              </div>
            </span>
          </div>

          <div className="set-row">
            <span className="k">系统补充提示词</span>
            <span className="v">
              <textarea
                rows={3}
                className="preset-prompt-input"
                value={cfg.preset_prompt || ""}
                placeholder="例如：优先使用分点条列；重点标记 Action Items 和责任人；行业术语保持英文缩写不翻译..."
                onChange={(e) => patch({ preset_prompt: e.target.value })}
                onBlur={() => void persist(cfg)}
              />
              <div className="note">在生成会议纪要时，作为核心指令补充给 LLM 模型。</div>
            </span>
          </div>
        </div>

        {/* 4. 会议本地存档 */}
        <div id="set-machine" className="section-block">
          <h2 className="section">
            <span className="section-title-tag">会议本地存档</span>
          </h2>

          {report && (
            <>
              <div className="check">
                <span className={report.db_ok ? "st ok" : "st no"}>
                  {report.db_ok ? "正常" : "异常"}
                </span>
                <span className="d">本地数据库与凭据管理器 · {report.db_message}</span>
              </div>
              <p className="note">数据主目录：{report.data_dir}</p>
            </>
          )}

          {/* 归档会议管理 */}
          <div className="subsection-title" style={{ marginTop: 24 }}>
            已归档会议 ({archivedList.length})
          </div>
          <p className="note" style={{ marginTop: 0 }}>
            归档会议已从主列表收起，所有录音、逐字稿与纪要数据完整留存。
          </p>

          {archivedList.length === 0 ? (
            <div className="note" style={{ padding: "8px 0" }}>
              暂无已归档的会议。在侧边栏会议上右键选择「归档会议」即可放入此库。
            </div>
          ) : (
            <div className="archived-list">
              {archivedList.map((m) => (
                <div className="archived-item" key={m.id}>
                  <div className="archived-info">
                    <div className="archived-title" title={m.title}>
                      {m.title}
                    </div>
                    <div className="archived-meta data">
                      {m.started_at.slice(0, 16).replace("T", " ")} · {formatTs(m.duration_ms)}
                    </div>
                  </div>
                  <div className="archived-actions">
                    <button
                      className="act"
                      style={{ padding: "4px 10px", fontSize: 12 }}
                      onClick={() => void handleRestore(m.id, m.title)}
                      title="恢复到侧边栏主列表"
                    >
                      恢复
                    </button>
                    <button
                      className="act"
                      style={{ padding: "4px 10px", fontSize: 12, color: "var(--live)" }}
                      onClick={() => void handleDelete(m.id, m.title)}
                      title="彻底删除并清理磁盘数据"
                    >
                      彻底删除
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>

      {/* 右侧固定的快速导航 */}
      <QuickNav items={settingsNavItems} />
    </div>
  );
}
