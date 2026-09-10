import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import { api, asMessage, events } from "./api";
import type { Meeting } from "./types";
import { formatTs } from "./types";
import Session from "./views/Session";
import MeetingView from "./views/MeetingView";
import Settings from "./views/Settings";
import MeetingContextMenu from "./MeetingContextMenu";
import ParticipantModal from "./components/ParticipantModal";

const PAGE = 20;

/** 路径的小写扩展名，不含点。拿不到就返回空串。 */
function extensionOf(path: string): string {
  const dot = path.lastIndexOf(".");
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return dot > cut ? path.slice(dot + 1).toLowerCase() : "";
}

/** 只取文件名。拖进来的是绝对路径，报错时刷一整行路径太吵。 */
function fileNameOf(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut >= 0 ? path.slice(cut + 1) : path;
}

/** 主区当前展示什么。录制永远发生在 session 视图里。 */
type Stage =
  | { kind: "session" }
  | { kind: "meeting"; id: number }
  | { kind: "settings" };

export default function App() {
  const [stage, setStage] = useState<Stage>({ kind: "session" });
  const [meetings, setMeetings] = useState<Meeting[]>([]);
  const [total, setTotal] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; meeting: Meeting } | null>(null);

  // 全局屏蔽系统/WebView 默认自带的右键菜单
  useEffect(() => {
    const handleContextMenu = (e: MouseEvent) => {
      e.preventDefault();
    };
    window.addEventListener("contextmenu", handleContextMenu);
    return () => window.removeEventListener("contextmenu", handleContextMenu);
  }, []);

  // 提示信息 4 秒后自动隐去
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), 4000);
    return () => clearTimeout(timer);
  }, [notice]);

  /** 正在录制的会议 id。同一时刻至多一场。 */
  const [liveId, setLiveId] = useState<number | null>(null);
  const [liveTitle, setLiveTitle] = useState("");
  const [liveSince, setLiveSince] = useState<number | null>(null);
  const [liveElapsed, setLiveElapsed] = useState(0);

  /** 正在整理的会议 id —— 用户只需要知道「在整理」。 */
  const [busyId, setBusyId] = useState<number | null>(null);
  /** 参会人管理弹窗显隐 */
  const [showParticipants, setShowParticipants] = useState(false);
  /** 记录各会议当前的整理阶段（transcribing / summarizing） */
  const [meetingPhases, setMeetingPhases] = useState<Record<number, "transcribing" | "summarizing">>({});

  /** 可导入的扩展名，由后端给。前端不另维护一份，免得两边说法不一致。 */
  const [audioExts, setAudioExts] = useState<string[]>([]);
  /** 文件正悬在窗口上方。 */
  const [hovering, setHovering] = useState(false);

  const loadFirstPage = useCallback(async () => {
    try {
      const page = await api.listMeetingsPage(0, PAGE);
      setMeetings(page.items);
      setTotal(page.total);
    } catch (e) {
      setError(asMessage(e));
    }
  }, []);

  useEffect(() => {
    void api.importableExtensions().then(setAudioExts).catch(() => setAudioExts([]));
  }, []);

  useEffect(() => {
    void loadFirstPage();
    void api.recordingStatus().then((id) => {
      if (id !== null) {
        setLiveId(id);
        setLiveSince(Date.now());
      }
    });
  }, [loadFirstPage]);

  useEffect(() => {
    if (liveSince === null) return;
    const t = window.setInterval(
      () => setLiveElapsed(Math.floor((Date.now() - liveSince) / 1000)),
      1000
    );
    return () => window.clearInterval(t);
  }, [liveSince]);

  useEffect(() => {
    const off: Array<() => void> = [];
    void events
      .onProcessProgress((e) => {
        if (e.phase === "transcribing" || e.phase === "importing") {
          setMeetingPhases((cur) => ({ ...cur, [e.meeting_id]: "transcribing" }));
        } else if (e.phase === "summarizing") {
          setMeetingPhases((cur) => ({ ...cur, [e.meeting_id]: "summarizing" }));
        }
      })
      .then((f) => off.push(f));

    void events
      .onProcessDone((e) => {
        setBusyId(null);
        setMeetingPhases((cur) => {
          const next = { ...cur };
          delete next[e.meeting_id];
          return next;
        });
        void loadFirstPage();
        if (e.phase === "failed") setError(e.detail);
      })
      .then((f) => off.push(f));
    return () => off.forEach((f) => f());
  }, [loadFirstPage]);

  const startImport = useCallback(
    async (path: string) => {
      try {
        const id = await api.importAudio({ path });
        setBusyId(id);
        setStage({ kind: "session" });
        await loadFirstPage();
      } catch (e) {
        setError(asMessage(e));
      }
    },
    [loadFirstPage]
  );

  // 拖放的判断依赖 liveId / busyId / audioExts，而监听只想订阅一次。
  // 把最新的处理函数放进 ref，回调里永远读到当前这一版。
  const onDrop = useRef<(paths: string[]) => void>(() => {});
  onDrop.current = (paths) => {
    // 清单还没取回来（后端刚起、或那次 invoke 失败）时不做前端过滤，
    // 直接交给后端判断——宁可多一次往返，也别把能导的文件挡在门外。
    const audio =
      audioExts.length === 0
        ? paths
        : paths.filter((p) => audioExts.includes(extensionOf(p)));
    if (audio.length === 0) {
      setError(`这不是能识别的音频。支持：${audioExts.join(" / ")}`);
      return;
    }
    if (liveId !== null) {
      setError("正在录制，先结束当前会议再导入");
      return;
    }
    if (busyId !== null) {
      setError("上一场会议还在整理中，等它完成再导入");
      return;
    }
    // 转写是单任务串行的（§6），多拖几个也只能一个一个来。
    if (audio.length > 1) {
      setError(`一次只处理一个音频，先从「${fileNameOf(audio[0])}」开始`);
    }
    void startImport(audio[0]);
  };

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let cancelled = false;
    void getCurrentWebview()
      .onDragDropEvent((e) => {
        if (e.payload.type === "enter" || e.payload.type === "over") setHovering(true);
        else if (e.payload.type === "leave") setHovering(false);
        else if (e.payload.type === "drop") {
          setHovering(false);
          onDrop.current(e.payload.paths);
        }
      })
      .then((f) => {
        if (cancelled) f();
        else unlisten = f;
      })
      .catch((e) => setError(asMessage(e)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const loadMore = async () => {
    setLoadingMore(true);
    try {
      const page = await api.listMeetingsPage(meetings.length, PAGE);
      setMeetings((cur) => [...cur, ...page.items]);
      setTotal(page.total);
    } catch (e) {
      setError(asMessage(e));
    } finally {
      setLoadingMore(false);
    }
  };

  const onStarted = (id: number, title: string) => {
    setLiveId(id);
    setLiveTitle(title);
    setLiveSince(Date.now());
    setLiveElapsed(0);
    void loadFirstPage();
  };

  const onEnded = (id: number) => {
    setLiveId(null);
    setLiveSince(null);
    setBusyId(id);
    void loadFirstPage();
  };

  const pad = (n: number) => String(n).padStart(2, "0");
  const liveClock = `${pad(Math.floor(liveElapsed / 60))}:${pad(liveElapsed % 60)}`;
  const showPin = liveId !== null && stage.kind !== "session";

  const statusText = (m: Meeting) => {
    if (m.id === liveId) return "录制中";
    if (m.id === busyId || m.status === "processing") return "整理中";
    if (m.status === "failed") return "整理失败";
    return formatTs(m.duration_ms);
  };

  const handleShare = async (m: Meeting) => {
    try {
      const text = await api.getMeetingShareText(m.id);
      await navigator.clipboard.writeText(text);
      setError(null);
      setNotice(`已复制「${m.title}」纪要到剪贴板`);
    } catch (e) {
      setError(`分享失败：${asMessage(e)}`);
    }
  };

  const handleExportAudio = async (m: Meeting) => {
    try {
      const path = await save({
        defaultPath: `${m.title}_录音.wav`,
        filters: [{ name: "音频文件", extensions: ["wav", "aac", "m4a", "mp3"] }],
      });
      if (path) {
        const out = await api.exportMeetingAudio(m.id, path);
        setNotice(`音频源已成功导出至：${out}`);
      }
    } catch (e) {
      setError(`导出音频失败：${asMessage(e)}`);
    }
  };

  const handleExportTranscript = async (m: Meeting) => {
    try {
      const path = await save({
        defaultPath: `${m.title}_逐字稿.md`,
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (path) {
        const out = await api.exportTranscriptMarkdown(m.id, path);
        setNotice(`逐字稿已成功导出至：${out}`);
      }
    } catch (e) {
      setError(`导出逐字稿失败：${asMessage(e)}`);
    }
  };

  const handleExportSummary = async (m: Meeting) => {
    try {
      const path = await save({
        defaultPath: `${m.title}_会议纪要.md`,
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (path) {
        const out = await api.exportSummaryMarkdown(m.id, path);
        setNotice(`会议纪要已成功导出至：${out}`);
      }
    } catch (e) {
      setError(`导出会议纪要失败：${asMessage(e)}`);
    }
  };

  const handleArchive = async (m: Meeting) => {
    try {
      await api.archiveMeeting(m.id, true);
      setNotice(`已归档「${m.title}」，可在设置中查看归档清单`);
      if (stage.kind === "meeting" && stage.id === m.id) {
        setStage({ kind: "session" });
      }
      await loadFirstPage();
    } catch (e) {
      setError(`归档失败：${asMessage(e)}`);
    }
  };

  return (
    <div className="shell">
      <aside className="rail">
        <div className="rail-head">
          <h1>VocMeet</h1>
          <div className="rail-head-actions">
            <button
              className={showParticipants ? "gear on" : "gear"}
              title="参会人管理"
              aria-label="参会人管理"
              onClick={() => setShowParticipants(true)}
            >
              <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor"
                   strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
                <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" />
                <circle cx="9" cy="7" r="4" />
                <path d="M22 21v-2a4 4 0 0 0-3-3.87" />
                <path d="M16 3.13a4 4 0 0 1 0 7.75" />
              </svg>
            </button>
            <button
              className={stage.kind === "settings" ? "gear on" : "gear"}
              title="设置"
              aria-label="设置"
              onClick={() => setStage({ kind: "settings" })}
            >
              <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor"
                   strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="3" />
                <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
              </svg>
            </button>
          </div>
        </div>

        <button
          className="new-meeting"
          disabled={liveId !== null}
          title={liveId !== null ? "先结束当前会议" : undefined}
          onClick={() => setStage({ kind: "session" })}
        >
          ＋ 新建会议
        </button>

        <div className="rail-list">
          <span className="label">会议 {total > 0 ? total : ""}</span>
          {meetings.length === 0 && <div className="hollow">还没有会议</div>}
          {meetings.map((m) => {
            const isLive = m.id === liveId;
            const phase = meetingPhases[m.id] || (m.id === busyId || m.status === "processing" ? "transcribing" : null);

            return (
              <button
                key={m.id}
                className={stage.kind === "meeting" && stage.id === m.id ? "entry on" : "entry"}
                onClick={() =>
                  m.id === liveId
                    ? setStage({ kind: "session" })
                    : setStage({ kind: "meeting", id: m.id })
                }
                onContextMenu={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  setMenu({ x: e.clientX, y: e.clientY, meeting: m });
                }}
              >
                <div className="entry-head-line">
                  <span className="t">{m.title}</span>
                  {isLive && (
                    <span className="rail-tag live">
                      <span className="status-dot live" />
                      <span>录制中</span>
                    </span>
                  )}
                  {!isLive && phase === "transcribing" && (
                    <span className="rail-tag transcribing">
                      <span className="status-dot pulsing" />
                      <span>解析录音中</span>
                    </span>
                  )}
                  {!isLive && phase === "summarizing" && (
                    <span className="rail-tag summarizing">
                      <span className="status-dot pulsing" />
                      <span>生成会议纪要中</span>
                    </span>
                  )}
                </div>
                <span className="m data">
                  {m.started_at.slice(5, 16).replace("T", " ")} · {statusText(m)}
                </span>
              </button>
            );
          })}
          {meetings.length < total && (
            <button className="more" disabled={loadingMore} onClick={() => void loadMore()}>
              {loadingMore ? "加载中" : `加载更多 · 还有 ${total - meetings.length} 场`}
            </button>
          )}
        </div>
      </aside>

      <main className="stage">
        {error && (
          <div className="banner">
            <span>{error}</span>
            <button onClick={() => setError(null)} aria-label="关闭">×</button>
          </div>
        )}
        {notice && (
          <div
            className="msg"
            style={{
              margin: "14px 24px 0",
              color: "var(--signal)",
              borderBottom: "1px solid var(--rule)",
              paddingBottom: "8px",
            }}
          >
            {notice}
          </div>
        )}

        <div className="stage-body">
          {stage.kind === "session" && (
            <Session
              liveId={liveId}
              liveTitle={liveTitle}
              liveElapsed={liveElapsed}
              busyId={busyId}
              audioExts={audioExts}
              onStarted={onStarted}
              onEnded={onEnded}
              onImport={startImport}
              onOpenMeeting={(id) => setStage({ kind: "meeting", id })}
              onError={setError}
            />
          )}
          {stage.kind === "meeting" && (
            <MeetingView
              key={stage.id}
              meetingId={stage.id}
              onError={setError}
              onChanged={loadFirstPage}
              onBusy={setBusyId}
            />
          )}
          {stage.kind === "settings" && (
            <Settings onError={setError} onMeetingChanged={loadFirstPage} />
          )}
        </div>

        {hovering && (
          <div className="drop-veil">
            <div className="drop-inner">
              <p className="drop-line">松手，把这段音频变成一场会议</p>
              <div className="baseline" />
              <span className="data">{audioExts.join(" · ")}</span>
            </div>
          </div>
        )}

        {showPin && (
          <div className="pin" onClick={() => setStage({ kind: "session" })}>
            <span className="dot" />
            <span className="txt">正在录制 · {liveTitle || "未命名会议"}</span>
            <span className="data">{liveClock}</span>
            <span className="back">回到录制</span>
          </div>
        )}
      </main>

      {menu && (
        <MeetingContextMenu
          x={menu.x}
          y={menu.y}
          meeting={menu.meeting}
          onClose={() => setMenu(null)}
          onShare={handleShare}
          onExportAudio={handleExportAudio}
          onExportTranscript={handleExportTranscript}
          onExportSummary={handleExportSummary}
          onArchive={handleArchive}
        />
      )}

      {showParticipants && (
        <ParticipantModal
          onClose={() => setShowParticipants(false)}
          onError={setError}
        />
      )}
    </div>
  );
}
