import { useCallback, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { api, asMessage, events } from "../api";
import type { MeetingDetail, ProcessEvent, SpeakerRow, Utterance } from "../types";
import { formatTs } from "../types";

const NL = String.fromCharCode(10);

interface Props {
  meetingId: number;
  onError: (msg: string) => void;
  onChanged: () => void;
  /** 重整开始时告诉外层，好让侧栏跟着显示「整理中」。 */
  onBusy: (id: number) => void;
}

/** 一场已完成的会议：先听，再读，最后带走。 */
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
  /** 参会人数。空 = 让聚类自己估，但阈值 0.5 偏低会过分裂，长会议尤其明显。 */
  const [speakers2, setSpeakers2] = useState("");
  const audio = useRef<HTMLAudioElement>(null);

  const load = useCallback(async () => {
    try {
      const detail = await api.getMeetingDetail(meetingId);
      setD(detail);
      if (detail.utterance_count > 0) {
        const [u, s] = await Promise.all([
          api.getTranscript(meetingId),
          api.listSpeakers(meetingId),
        ]);
        setUtterances(u);
        setSpeakers(s);
        setNames(Object.fromEntries(s.map((r) => [r.key, r.display_name ?? ""])));
      }
    } catch (e) {
      onError(asMessage(e));
    }
  }, [meetingId, onError]);

  useEffect(() => {
    void load();
    void api.meetingHasAudio(meetingId).then(setHasAudio).catch(() => setHasAudio(false));
  }, [load, meetingId]);

  // 重整的进度事件是全局的，只认自己这一场。
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
        void load();
        onChanged();
      })
      .then((f) => off.push(f));
    return () => off.forEach((f) => f());
  }, [meetingId, load, onChanged, onError]);

  if (!d) return <div className="hollow">读取中</div>;

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
      await load();
      onChanged();
    } catch (e) {
      onError(asMessage(e));
    }
  };

  /**
   * 重新整理。
   *
   * `full` 从盘上的录音重跑识别与纪要，`summary` 只重写纪要。
   * 两种都按**点下去这一刻**的配置来：换了模型再点一次，就是换个模型重写。
   */
  const reprocess = async (mode: "full" | "summary") => {
    const n = Number(speakers2);
    const known = mode === "full" && Number.isInteger(n) && n > 0 ? n : null;
    try {
      await api.reprocessMeeting(meetingId, mode, known);
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

  return (
    <div className="doc">
      <div className="meeting-head">
        <h2>{d.meeting.title}</h2>
        <span className="data">
          {started} · {formatTs(d.meeting.duration_ms)} · {d.attendees.length} 人 ·{" "}
          {d.utterance_count} 条发言
        </span>
      </div>

      {src && (
        <div className="player">
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
                <rect width="3.5" height="13" /><rect x="8.5" width="3.5" height="13" />
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
          <span className="data">{clock(at)} / {clock(dur)}</span>
        </div>
      )}

      {d.keywords.length > 0 && (
        <div className="chips">
          {d.keywords.map((k) => (
            <span className="chip key" key={k}>{k}</span>
          ))}
        </div>
      )}

      {d.attendees.length > 0 && (
        <p className="people">
          参会 <b>{d.attendees.join("、")}</b>
        </p>
      )}

      {d.note.trim() !== "" && (
        <>
          <h2 className="section">随手记</h2>
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
        </>
      )}

      <h2 className="section">会议纪要</h2>
      {step ? (
        <div className="fetching">
          <p className="data" style={{ margin: 0 }}>
            {step.phase === "transcribing" ? "重新识别录音" : "重写纪要"}
            {step.detail ? ` · ${step.detail}` : ""}
          </p>
          <p className="note" style={{ marginTop: 6 }}>
            用的是现在这份配置——模型、聚类阈值、出网策略都读当前值。完成后这一页自己刷新。
          </p>
        </div>
      ) : d.summary ? (
        <div className="prose">{d.summary}</div>
      ) : (
        <div className="hollow">
          {d.meeting.status === "processing" ? "正在整理" : "这场会议还没有纪要"}
        </div>
      )}

      <div className="actions">
        {d.summary && (
          <>
            <button className="act" onClick={() => void saveMarkdown()}>另存为 Markdown</button>
            <button className="act" onClick={() => window.print()}>另存为 PDF</button>
          </>
        )}
        {utterances.length > 0 && (
          <button
            className="act"
            disabled={step !== null}
            title="逐字稿不动，只用当前配置的模型重写一遍纪要"
            onClick={() => void reprocess("summary")}
          >
            重写纪要
          </button>
        )}
        {hasAudio && (
          <>
            <label className="headcount">
              参会人数
              <input
                type="number"
                min={1}
                max={64}
                value={speakers2}
                placeholder="不填就自己猜"
                disabled={step !== null}
                onChange={(e) => setSpeakers2(e.target.value)}
              />
            </label>
            <button
              className="act"
              disabled={step !== null}
              title="从盘上的录音重跑：识别 → 说话人分离 → 纪要"
              onClick={() => void reprocess("full")}
            >
              重新解析录音
            </button>
          </>
        )}
      </div>
      {(utterances.length > 0 || hasAudio) && !step && (
        <p className="note">
          对这版纪要不满意就重写一遍。换个模型再点，就是换个模型重写——
          按下去那一刻的设置说了算。旧版本不会被删，界面显示最新的一版。
          {hasAudio && speakers.length > 6 && (
            <>
              <br />
              这场切出了 {speakers.length} 个说话人，多半是聚类过分裂了。
              填上真实参会人数再解析一次，会准得多。
            </>
          )}
        </p>
      )}

      {utterances.length > 0 && (
        <details className="fold">
          <summary>逐字稿 · {utterances.length} 条</summary>

          {speakers.length > 0 && (
            <div style={{ margin: "14px 0 22px" }}>
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

          {utterances.map((u) => (
            <div className={u.low_confidence ? "line doubt" : "line"} key={u.id}>
              <div className="who">
                <b>{u.speaker_name ?? u.speaker_id}</b>
                <span className="data">{formatTs(u.start_ms)}</span>
              </div>
              <div className="said">{u.text}</div>
            </div>
          ))}
        </details>
      )}
    </div>
  );
}
