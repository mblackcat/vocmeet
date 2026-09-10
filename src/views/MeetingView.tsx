import { useCallback, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { api, asMessage, events } from "../api";
import type { MeetingDetail, ProcessEvent, SpeakerRow, Utterance } from "../types";
import { formatTs } from "../types";
import MarkdownRenderer from "../components/MarkdownRenderer";
import AttendeesFold from "../components/AttendeesFold";
import QuickNav from "../components/QuickNav";

const NL = String.fromCharCode(10);

interface Props {
  meetingId: number;
  onError: (msg: string) => void;
  onChanged: () => void;
  /** 重整开始时告诉外层，好让侧栏跟着显示「整理中」。 */
  onBusy: (id: number) => void;
}

export default function MeetingView({ meetingId, onError, onChanged, onBusy }: Props) {
  const [d, setD] = useState<MeetingDetail | null>(null);
  const [utterances, setUtterances] = useState<Utterance[]>([]);
  const [speakers, setSpeakers] = useState<SpeakerRow[]>([]);
  const [names, setNames] = useState<Record<string, string>>({});
  const [playing, setPlaying] = useState(false);
  const [at, setAt] = useState(0);
  const [dur, setDur] = useState(0);
  const [hasAudio, setHasAudio] = useState(false);
  const [step, setStep] = useState<ProcessEvent | null>(null);
  const [notesOpen, setNotesOpen] = useState(true);
  const [transcriptOpen, setTranscriptOpen] = useState(true);

  const audio = useRef<HTMLAudioElement>(null);
  const curMeetingIdRef = useRef<number>(meetingId);
  curMeetingIdRef.current = meetingId;

  // 避免切换会议时的旧数据残留与异步竞态（修复串会Bug）
  const load = useCallback(async (targetId: number) => {
    try {
      const detail = await api.getMeetingDetail(targetId);
      if (curMeetingIdRef.current !== targetId) return;
      setD(detail);

      if (detail.utterance_count > 0) {
        const [u, s] = await Promise.all([
          api.getTranscript(targetId),
          api.listSpeakers(targetId),
        ]);
        if (curMeetingIdRef.current !== targetId) return;
        setUtterances(u);
        setSpeakers(s);
        setNames(Object.fromEntries(s.map((r) => [r.key, r.display_name ?? ""])));
      } else {
        setUtterances([]);
        setSpeakers([]);
        setNames({});
      }
    } catch (e) {
      if (curMeetingIdRef.current === targetId) {
        onError(asMessage(e));
      }
    }
  }, [onError]);

  useEffect(() => {
    // 切换会议时立即清理前一场会议的状态，杜绝串会
    setD(null);
    setUtterances([]);
    setSpeakers([]);
    setNames({});
    setStep(null);
    setPlaying(false);
    setAt(0);
    setDur(0);

    void load(meetingId);
    void api.meetingHasAudio(meetingId).then((has) => {
      if (curMeetingIdRef.current === meetingId) setHasAudio(has);
    }).catch(() => {
      if (curMeetingIdRef.current === meetingId) setHasAudio(false);
    });
  }, [meetingId, load]);

  // 重整的进度事件是全局的，只认自己这一场
  useEffect(() => {
    const off: Array<() => void> = [];
    void events
      .onProcessProgress((e) => {
        if (e.meeting_id === meetingId) setStep(e);
      })
      .then((f) => off.push(f));
    void events
      .onProcessDone((e) => {
        if (e.meeting_id !== meetingId) return;
        setStep(null);
        if (e.phase === "failed") onError(e.detail);
        void load(meetingId);
        onChanged();
      })
      .then((f) => off.push(f));
    return () => off.forEach((f) => f());
  }, [meetingId, load, onChanged, onError]);

  if (!d) return <div className="hollow">读取中...</div>;

  const src = d.playback_path ? convertFileSrc(d.playback_path) : null;

  const toggle = () => {
    const el = audio.current;
    if (!el) return;
    if (el.paused) {
      void el.play();
      setPlaying(true);
    } else {
      el.pause();
      setPlaying(false);
    }
  };

  const seek = (e: React.MouseEvent<HTMLDivElement>) => {
    const el = audio.current;
    if (!el || !dur) return;
    const box = e.currentTarget.getBoundingClientRect();
    el.currentTime = ((e.clientX - box.left) / box.width) * dur;
  };

  const saveName = async (key: string) => {
    const v = (names[key] ?? "").trim();
    if (!v) return;
    try {
      await api.nameSpeaker(meetingId, key, v);
      await load(meetingId);
      onChanged();
    } catch (e) {
      onError(asMessage(e));
    }
  };

  /**
   * 重新整理。
   * mode: "full" 从盘上的录音重跑识别；"summary" 只重写纪要。
   * 参会人数量参数彻底移除，按自动解析逻辑走。
   */
  const reprocess = async (mode: "full" | "summary") => {
    try {
      await api.reprocessMeeting(meetingId, mode, null);
      onBusy(meetingId);
      setStep({
        meeting_id: meetingId,
        phase: mode === "full" ? "transcribing" : "summarizing",
        progress: 0,
        detail: mode === "full" ? "重新识别" : "重写纪要",
      });
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const saveMarkdown = async () => {
    try {
      const path = await save({
        defaultPath: `${d.meeting.title}.md`,
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (path) await api.exportMarkdown(meetingId, path);
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const clock = (s: number) => {
    const p = (n: number) => String(Math.floor(n)).padStart(2, "0");
    return `${p(s / 60)}:${p(s % 60)}`;
  };

  const started = d.meeting.started_at.slice(0, 16).replace("T", " ");

  // 状态动效与判断：录音解析中 / 纪要生成中
  const isTranscribing =
    step?.phase === "transcribing" ||
    (step === null && d.meeting.status === "processing" && utterances.length === 0);
  const isSummarizing =
    step?.phase === "summarizing" ||
    (step === null && d.meeting.status === "processing" && utterances.length > 0 && !d.summary);

  // 逐字稿总字符数统计
  const totalCharacters = utterances.reduce((acc, u) => acc + (u.text ? u.text.length : 0), 0);

  // 右侧快速导航项：录音、纪要、随手记、逐字稿
  const navItems = [
    ...(src ? [{ id: "section-player", label: "录音" }] : []),
    { id: "section-summary", label: "纪要" },
    ...(d.note.trim() !== "" ? [{ id: "section-notes", label: "随手记" }] : []),
    { id: "section-transcript", label: "逐字稿" },
  ];

  return (
    <div className="doc-wrapper">
      <div className="doc">
        {/* 会议标题与基本信息 */}
        <div className="meeting-head" id="section-header">
          <h2>{d.meeting.title}</h2>
          <span className="data">
            {started} · {formatTs(d.meeting.duration_ms)} · {d.attendees.length} 人 ·{" "}
            {d.utterance_count} 条发言
          </span>
        </div>

        {/* 录音播放器 */}
        {src && (
          <div className="player" id="section-player">
            <audio
              ref={audio}
              src={src}
              onLoadedMetadata={(e) => setDur(e.currentTarget.duration || 0)}
              onTimeUpdate={(e) => setAt(e.currentTarget.currentTime)}
              onEnded={() => setPlaying(false)}
            />
            <button className="play" onClick={toggle} aria-label={playing ? "暂停" : "播放"}>
              {playing ? (
                <svg width="12" height="13" viewBox="0 0 12 13" fill="currentColor">
                  <rect width="3.5" height="13" />
                  <rect x="8.5" width="3.5" height="13" />
                </svg>
              ) : (
                <svg width="12" height="13" viewBox="0 0 12 13" fill="currentColor">
                  <path d="M0 0l12 6.5L0 13z" />
                </svg>
              )}
            </button>
            <div className="scrub" onClick={seek}>
              <div className="track" />
              <div className="fill" style={{ width: dur ? `${(at / dur) * 100}%` : 0 }} />
              <div className="head" style={{ left: dur ? `${(at / dur) * 100}%` : 0 }} />
            </div>
            <span className="data">
              {clock(at)} / {clock(dur)}
            </span>
          </div>
        )}

        {/* 关键词 */}
        {d.keywords.length > 0 && (
          <div className="chips">
            {d.keywords.map((k) => (
              <span className="chip key" key={k}>
                {k}
              </span>
            ))}
          </div>
        )}

        {/* 参会人折叠展示（需求2：最多3行，渐隐，hover tips，点击展开与收起） */}
        <AttendeesFold attendees={d.attendees} />

        {/* 会议纪要区域 */}
        <div id="section-summary" className="section-block">
          <div className="section-title-row">
            <h2 className="section">
              <span className="section-title-tag">会议纪要</span>
            </h2>

            {/* 正在生成纪要时隐藏按钮并展示占位动效 */}
            {!isSummarizing && (
              <button
                type="button"
                className="icon-btn-circle"
                title="重新生成"
                aria-label="重新生成"
                disabled={step !== null}
                onClick={() => void reprocess("summary")}
              >
                <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <path d="M3 12a9 9 0 0 1 15-6.7L21 8" />
                  <path d="M21 3v5h-5" />
                  <path d="M21 12a9 9 0 0 1-15 6.7L3 16" />
                  <path d="M3 21v-5h5" />
                </svg>
              </button>
            )}
          </div>

          {/* 生成中占位 svg 动效 */}
          {isSummarizing ? (
            <div className="fetching-placeholder">
              <div className="scanning-wave">
                <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                  <path d="M12 3v3M12 18v3M4.93 4.93l2.12 2.12M16.95 16.95l2.12 2.12M3 12h3M18 12h3M4.93 19.07l2.12-2.12M16.95 7.05l2.12-2.12" className="spin-slow" />
                  <circle cx="12" cy="12" r="4" strokeDasharray="3 3" />
                </svg>
              </div>
              <p className="data" style={{ margin: "8px 0 2px" }}>
                正在提炼会议纪要...
              </p>
              <span className="note">{step?.detail || "根据发言全文生成段落、要点与行动项"}</span>
            </div>
          ) : d.summary ? (
            <MarkdownRenderer content={d.summary} />
          ) : (
            <div className="hollow">
              {d.meeting.status === "processing" ? "正在整理" : "这场会议还没有纪要"}
            </div>
          )}

          {/* 另存为两个按钮单独一行（需求4） */}
          {d.summary && !isSummarizing && (
            <div className="export-actions-row">
              <button className="act" onClick={() => void saveMarkdown()}>
                另存为 Markdown
              </button>
              <button className="act" onClick={() => window.print()}>
                另存为 PDF
              </button>
            </div>
          )}
        </div>

        {/* 随手记区域（需求10：挪到纪要后、逐字稿前，支持整段收起和展开） */}
        {d.note.trim() !== "" && (
          <div id="section-notes" className="section-block">
            <div
              className="section-title-row collapsible"
              onClick={() => setNotesOpen(!notesOpen)}
            >
              <div className="section-title-left">
                <h2 className="section">
                  <span className="section-title-tag">随手记</span>
                </h2>
              </div>
              <div className="section-title-actions">
                <button type="button" className="collapse-toggle-btn">
                  {notesOpen ? "收起 ↑" : "展开 ↓"}
                </button>
              </div>
            </div>

            {notesOpen && (
              <div className="jots">
                {d.note
                  .split(NL)
                  .map((l) => l.trim())
                  .filter((l) => l !== "")
                  .map((line, i) => {
                    const m = /^\[(\d{1,2}:\d{2})\]\s*(.*)$/.exec(line);
                    return (
                      <div className="jot" key={i}>
                        {m && <span className="data">{m[1]}</span>}
                        {m ? m[2] : line}
                      </div>
                    );
                  })}
              </div>
            )}
          </div>
        )}

        {/* 逐字稿区域（需求6：显示条数与字符数，增加刷新圆圈按钮，解析时svg占位动画并隐藏按钮） */}
        <div id="section-transcript" className="section-block">
          <div className="section-title-row">
            <div
              className="section-title-left collapsible"
              onClick={() => setTranscriptOpen(!transcriptOpen)}
            >
              <h2 className="section">
                <span className="section-title-tag">逐字稿</span>
              </h2>
              {utterances.length > 0 && (
                <span className="transcript-stats">
                  · {utterances.length} 条 · {totalCharacters.toLocaleString()} 字
                </span>
              )}
            </div>

            <div className="section-title-actions">
              <button
                type="button"
                className="collapse-toggle-btn"
                onClick={() => setTranscriptOpen(!transcriptOpen)}
              >
                {transcriptOpen ? "收起 ↑" : "展开 ↓"}
              </button>

              {/* 刷新按钮：hover后提示文字“重新解析录音”，解析录音时隐藏按钮 */}
              {hasAudio && !isTranscribing && (
                <button
                  type="button"
                  className="icon-btn-circle"
                  title="重新解析录音"
                  aria-label="重新解析录音"
                  disabled={step !== null}
                  onClick={() => void reprocess("full")}
                >
                  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                    <path d="M3 12a9 9 0 0 1 15-6.7L21 8" />
                    <path d="M21 3v5h-5" />
                    <path d="M21 12a9 9 0 0 1-15 6.7L3 16" />
                    <path d="M3 21v-5h5" />
                  </svg>
                </button>
              )}
            </div>
          </div>

          {/* 解析录音中占位 svg 动效 */}
          {isTranscribing ? (
            <div className="fetching-placeholder">
              <div className="scanning-wave">
                <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                  <path d="M2 10v4M6 6v12M10 3v18M14 8v8M18 5v14M22 10v4" className="wave-bars" />
                </svg>
              </div>
              <p className="data" style={{ margin: "8px 0 2px" }}>
                正在解析录音与声学特征...
              </p>
              <span className="note">{step?.detail || "分离说话人并转写发言中"}</span>
            </div>
          ) : (
            transcriptOpen &&
            utterances.length > 0 && (
              <div className="transcript-content">
                {speakers.length > 0 && (
                  <div className="speaker-namers">
                    {speakers.map((s) => (
                      <div className="namer" key={s.key}>
                        <code>{s.key}</code>
                        <span className="ex">{s.sample}</span>
                        <input
                          value={names[s.key] ?? ""}
                          placeholder="这是谁"
                          onChange={(e) => setNames({ ...names, [s.key]: e.target.value })}
                          onBlur={() => void saveName(s.key)}
                          onKeyDown={(e) => e.key === "Enter" && void saveName(s.key)}
                        />
                      </div>
                    ))}
                  </div>
                )}

                <div className="utterance-lines">
                  {utterances.map((u) => (
                    <div className={u.low_confidence ? "line doubt" : "line"} key={u.id}>
                      <div className="who">
                        <b>{u.speaker_name ?? u.speaker_id}</b>
                        <span className="data">{formatTs(u.start_ms)}</span>
                      </div>
                      <div className="said">{u.text}</div>
                    </div>
                  ))}
                </div>
              </div>
            )
          )}
        </div>
      </div>

      {/* 右侧固定的快速导航（需求1） */}
      <QuickNav items={navItems} />
    </div>
  );
}
