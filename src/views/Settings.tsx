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
} from "../types";
import { formatBytes, formatTs } from "../types";

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

interface ProviderPreset {
  id: string;
  name: string;
  desc: string;
  apiBase: string;
  defaultModel: string;
  candidateModels: string[];
  contextTokens: number;
  requiresKey: boolean;
  keyHint: string;
  egressPolicy: "local_only" | "open";
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
    id: "gemini",
    name: "Google Gemini",
    desc: "官方 OpenAI 兼容接口，百万上下文，极速稳定",
    apiBase: "https://generativelanguage.googleapis.com/v1beta/openai",
    defaultModel: "gemini-2.0-flash",
    candidateModels: ["gemini-2.0-flash", "gemini-1.5-flash", "gemini-1.5-pro"],
    contextTokens: 65536,
    requiresKey: true,
    keyHint: "请在 Google AI Studio 获取 API Key (AIzaSy...)",
    egressPolicy: "open",
  },
  {
    id: "deepseek",
    name: "DeepSeek",
    desc: "深度求索官方开放平台，高性价比",
    apiBase: "https://api.deepseek.com/v1",
    defaultModel: "deepseek-chat",
    candidateModels: ["deepseek-chat"],
    contextTokens: 65536,
    requiresKey: true,
    keyHint: "在 DeepSeek 开放平台控制台获取 API Key",
    egressPolicy: "open",
  },
  {
    id: "openai",
    name: "OpenAI / 中转",
    desc: "标准 OpenAI 兼容服务",
    apiBase: "https://api.openai.com/v1",
    defaultModel: "gpt-4o-mini",
    candidateModels: ["gpt-4o-mini", "gpt-4o"],
    contextTokens: 32768,
    requiresKey: true,
    keyHint: "填入 OpenAI 或中转网关的 Bearer Token",
    egressPolicy: "open",
  },
];

/** 设置与自检合并在一页：都是「这台机器上的事」，没必要分成两个入口。 */
export default function Settings({ onError, onMeetingChanged }: Props) {
  const [cfg, setCfg] = useState<AppConfig | null>(null);
  const [choices, setChoices] = useState<[string, string][]>([]);
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
    const off: Array<() => void> = [];
    void events.onModelsProgress(setFetching).then((f) => off.push(f));
    void events.onPullProgress(setPulling).then((f) => off.push(f));
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
        // 下完之后模型目录已经写回配置了，把两边都重新读一遍。
        void refreshDoctor();
        void api.getConfig().then(setCfg);
      })
      .then((f) => off.push(f));
    return () => off.forEach((f) => f());
     
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const [c, l, k, a] = await Promise.all([
          api.getConfig(),
          api.egressPolicyLabels(),
          api.hasApiKey(),
          api.auditCount(),
        ]);
        setCfg(c);
        setChoices(l);
        setHasKey(k);
        setAudits(a);
      } catch (e) {
        onError(asMessage(e));
      }
      try {
        const [r, d, mp] = await Promise.all([
          api.doctor(),
          api.listDevices().catch(() => [] as DeviceInfo[]),
          api.modelDownloadPlan(),
        ]);
        setReport(r);
        setDevices(d);
        setPlan(mp);
      } catch (e) {
        onError(asMessage(e));
      }
      // 端点体检要真发一次请求，放最后，别拖慢整页。
      try {
        const [d, sug] = await Promise.all([api.diagnoseLlm(), api.suggestedLlmModels()]);
        setLlm(d);
        setSuggested(sug);
      } catch (e) {
        onError(asMessage(e));
      }
      void loadArchived();
    })();
  }, [onError]);

  if (!cfg) return <div className="hollow">读取中</div>;

  const patch = (p: Partial<AppConfig>) => setCfg({ ...cfg, ...p });

  const persist = async (next: AppConfig) => {
    setCfg(next);
    try {
      await api.saveConfig(next);
      setMsg("已保存");
    } catch (e) {
      onError(asMessage(e));
    }
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

  /** 已经有一套权重的话直接指过去，别再下一遍 360MB。 */
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
        api_base: p.apiBase,
        model: p.defaultModel,
        context_tokens: p.contextTokens,
      },
    };
    await persist(next);
    setMsg(`已切换为「${p.name}」配置模板`);
    void api.diagnoseLlm().then(setLlm).catch(() => {});
  };

  return (
    <div className="doc">
      <div className="meeting-head">
        <h2>设置</h2>
        <span className="data">这台机器上的事</span>
      </div>

      {msg && <div className="msg">{msg}</div>}

      <h2 className="section">逐字稿去哪里</h2>
      {choices.map(([key, desc]) => (
        <label className="choice" key={key}>
          <input
            type="radio"
            checked={cfg.egress_policy === key}
            onChange={() =>
              void persist({ ...cfg, egress_policy: key as AppConfig["egress_policy"] })
            }
          />
          <span>
            <span className="c-t">{desc.split(" — ")[0]}</span>
            <span className="c-d"> {desc.split(" — ")[1] ?? ""}</span>
          </span>
        </label>
      ))}
      {cfg.egress_policy === "open" && (
        <div className="warn">
          生成纪要时逐字稿全文会发送到你配置的服务商。原始录音始终留在本机；
          说话人姓名会替换成「发言人A」这类代号再发出，生成后在本地换回真名。
        </div>
      )}
      <p className="note">已记录 {audits} 条出网记录，只有端点、模型和字符数，没有内容。</p>

      <h2 className="section">写纪要的模型</h2>

      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", margin: "14px 0 16px" }}>
        {PROVIDER_PRESETS.map((p) => {
          const active =
            (p.id === "ollama" && isLocalEndpoint(cfg.llm.api_base)) ||
            (p.id === "gemini" && cfg.llm.api_base.includes("googleapis.com")) ||
            (p.id === "deepseek" && cfg.llm.api_base.includes("deepseek.com")) ||
            (p.id === "openai" && cfg.llm.api_base.includes("openai.com"));
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

      {!isLocalEndpoint(cfg.llm.api_base) && cfg.egress_policy === "local_only" && (
        <div className="warn" style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
          <span>
            当前配置为远端服务地址，但出网策略限制为「仅本地端点」，会导致连接测试与纪要生成被拦截。
          </span>
          <button
            className="act"
            style={{ marginLeft: 16, whiteSpace: "nowrap", padding: "4px 10px" }}
            onClick={() => void persist({ ...cfg, egress_policy: "open" })}
          >
            一键切换为允许出网
          </button>
        </div>
      )}

      <div className="set-row">
        <span className="k">服务地址</span>
        <span className="v">
          <input
            type="text"
            value={cfg.llm.api_base}
            placeholder="http://localhost:11434/v1 或 https://generativelanguage.googleapis.com/v1beta/openai"
            onChange={(e) => patch({ llm: { ...cfg.llm, api_base: e.target.value } })}
            onBlur={() => void persist(cfg)}
          />
          <div className="note">
            {isLocalEndpoint(cfg.llm.api_base)
              ? "当前为本地端点（Ollama / vLLM / llama.cpp）"
              : "当前为远端端点（标准 OpenAI 兼容协议）"}
          </div>
        </span>
      </div>
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
            const currentPreset = PROVIDER_PRESETS.find(
              (p) =>
                (p.id === "gemini" && cfg.llm.api_base.includes("googleapis.com")) ||
                (p.id === "deepseek" && cfg.llm.api_base.includes("deepseek.com")) ||
                (p.id === "openai" && cfg.llm.api_base.includes("openai.com")) ||
                (p.id === "ollama" && isLocalEndpoint(cfg.llm.api_base)),
            );
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

          {llm && (
            <div className="note">
              {llm.installed.length > 0
                ? `端点返回可用模型：${llm.installed.join("、")}`
                : isLocalEndpoint(cfg.llm.api_base)
                  ? "端点上还没有任何模型"
                  : "远端服务未返回清单或已通过直接调用测试"}
            </div>
          )}
        </span>
      </div>
      {llm && (
        <div className="check">
          <span className={llm.reachable && llm.model_present ? "st ok" : "st no"}>
            {llm.reachable ? (llm.model_present ? "就绪" : "缺模型") : "连不上"}
          </span>
          <span className="d">
            {llm.message}
            {llm.hint && <div className="note">{llm.hint}</div>}
          </span>
        </div>
      )}

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
              由 Ollama 自己在下，关掉这一页也不会中断。
            </p>
          </div>
        ) : (
          llm?.reachable &&
          !llm.model_present && (
            <div className="fetching">
              <p className="note" style={{ marginTop: 0 }}>
                让 Ollama 直接拉一个，下完自动切过去：
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
              {suggested.map(([name, desc]) => (
                <div className="note" key={name}>
                  {name} — {desc}
                </div>
              ))}
            </div>
          )
        ))}

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
            长会议超出一半时会自动分段提要再合成（本地模型建议 16384，Gemini 等云端长文本模型建议 65536）
          </div>
        </span>
      </div>
      <div className="set-row">
        <span className="k">API 密钥</span>
        <span className="v">
          <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <input
              type="password"
              value={apiKey}
              placeholder={hasKey ? "已保存，留空则不改动" : "远端模型填写 API Token (如 AIzaSy...)"}
              onChange={(e) => setApiKey(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && apiKey.trim()) void storeKey();
              }}
              style={{ flex: 1 }}
            />
            <button
              className="act"
              disabled={!apiKey.trim()}
              onClick={() => void storeKey()}
              style={{ margin: 0, padding: "5px 12px", whiteSpace: "nowrap" }}
            >
              保存密钥
            </button>
          </div>
          <div className="note">
            {hasKey ? "系统凭据管理器已存有密钥。" : "尚未存储密钥。"}
            {(() => {
              const currentPreset = PROVIDER_PRESETS.find(
                (p) =>
                  (p.id === "gemini" && cfg.llm.api_base.includes("googleapis.com")) ||
                  (p.id === "deepseek" && cfg.llm.api_base.includes("deepseek.com")) ||
                  (p.id === "openai" && cfg.llm.api_base.includes("openai.com")),
              );
              return currentPreset ? ` ${currentPreset.keyHint}` : " 远端服务使用 Bearer 认证";
            })()}
          </div>
        </span>
      </div>
      <div className="actions" style={{ marginTop: 20, borderTop: 0, paddingTop: 0 }}>
        <button className="act" disabled={testing} onClick={() => void test()}>
          {testing ? "连接中" : "测试连接"}
        </button>
        <button className="act" disabled={probing} onClick={() => void probe()}>
          {probing ? "检查中" : "重新检查端点"}
        </button>
      </div>

      <h2 className="section">录音与识别</h2>
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
        </span>
      </div>
      <div className="set-row">
        <span className="k">分片时长</span>
        <span className="v">
          <input
            type="number"
            value={cfg.capture.chunk_seconds}
            onChange={(e) =>
              patch({ capture: { ...cfg.capture, chunk_seconds: Number(e.target.value) } })
            }
            onBlur={() => void persist(cfg)}
          />
          <div className="note">每隔这么久落一次盘，断电最多丢这么多</div>
        </span>
      </div>

      <h2 className="section">这台机器</h2>
      {report && (
        <>
          <div className="check">
            <span className={report.models_ok ? "st ok" : "st no"}>
              {report.models_ok ? "就绪" : "缺失"}
            </span>
            <span className="d">
              识别模型{" "}
              {report.models_ok
                ? `${report.total_model_mb.toFixed(0)} MB`
                : report.models_message.split(String.fromCharCode(10))[0]}
              <div className="note">{report.models_dir}</div>
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
                  会从 GitHub 下载 {formatBytes(plan.total_bytes)}（{plan.assets.join("、")}），
                  存到 {plan.models_dir}，下完自动生效。这是唯一一次需要联网的步骤。
                </p>
                <div className="actions" style={{ marginTop: 12, borderTop: 0, paddingTop: 0 }}>
                  <button className="act" onClick={() => void api.downloadModels()}>
                    一键下载模型
                  </button>
                  <button className="act" onClick={() => void pickModelsDir()}>
                    已经有了，指个目录
                  </button>
                </div>
              </div>
            )
          )}
          <div className="check">
            <span className={report.db_ok ? "st ok" : "st no"}>
              {report.db_ok ? "正常" : "异常"}
            </span>
            <span className="d">本地数据库与凭据管理器 · {report.db_message}</span>
          </div>
          <div className="check">
            <span className={report.loopback_ok ? "st ok" : "st no"}>
              {report.loopback_ok ? "可用" : "不可用"}
            </span>
            <span className="d">
              录制系统声音{report.loopback_ok ? "" : ` · ${report.loopback_message}`}
            </span>
          </div>
          {devices.map((dev, i) => (
            <div className="check" key={i}>
              <span className="st">{dev.direction}</span>
              <span className="d">
                {dev.name}
                {dev.is_default ? " · 默认" : ""}
              </span>
            </div>
          ))}
          <p className="note">数据存放在 {report.data_dir}</p>
        </>
      )}

      <h2 className="section">已归档会议 ({archivedList.length})</h2>
      <p className="note" style={{ marginTop: 0 }}>
        归档的会议会从侧边栏主列表中隐藏，所有录音、逐字稿与纪要数据完整保留。
      </p>
      {archivedList.length === 0 ? (
        <div className="note" style={{ padding: "10px 0" }}>
          暂无已归档的会议。在侧边栏会议记录上右键选择「归档会议」即可收纳至此。
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
  );
}
