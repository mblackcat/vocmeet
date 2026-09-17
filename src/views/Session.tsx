import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, asMessage, events } from "../api";
import type { ProcessEvent, TrackSource } from "../types";

interface Props {
  liveId: number | null;
  liveTitle: string;
  liveElapsed: number;
  busyId: number | null;
  /** 能导入的扩展名，给文件选择框做过滤。 */
  audioExts: string[];
  onStarted: (id: number, title: string) => void;
  onEnded: (id: number) => void;
  onImport: (path: string) => void;
  onOpenMeeting: (id: number) => void;
  onError: (msg: string) => void;
}

interface Jot {
  at: number;
  text: string;
}

const BARS = 13;

/** 电平表保留多少格历史。20Hz 下 48 格约 2.4 秒，读起来是一段音轨而不是跳动的柱子。 */
const METER_SLOTS = 48;

/** 两条轨的显示顺序与标签。 */
const TRACKS: Array<{ key: TrackSource; label: string }> = [
  { key: "Mic", label: "麦克风" },
  { key: "System", label: "系统" },
];

/**
 * rms → 条形高度（0..1）。
 *
 * 线性映射在会议语音这种小信号上几乎看不出动静——人耳是对数的，表也得是。
 * -60dB 铺满整个高度，低于这个当静音。
 */
function toHeight(rms: number): number {
  if (!(rms > 0)) return 0;
  const db = 20 * Math.log10(rms);
  return Math.min(1, Math.max(0, (db + 60) / 60));
}

/** 一条轨的滚动波形。高度由 rAF 直接写 DOM，不走 state——20Hz×2 轨的 setState 会把主线程拖垮。 */
function TrackMeter({
  label,
  silent,
  barsRef,
}: {
  label: string;
  silent: boolean;
  barsRef: (el: HTMLDivElement | null) => void;
}) {
  return (
    <div className={silent ? "meter-row silent" : "meter-row"}>
      <span className="meter-label">{label}</span>
      <div className="meter-bars" ref={barsRef} aria-hidden>
        {Array.from({ length: METER_SLOTS }, (_, i) => (
          <span key={i} />
        ))}
      </div>
    </div>
  );
}

const NL = String.fromCharCode(10);
const pad2 = (n: number) => String(Math.floor(n)).padStart(2, "0");
const stamp = (sec: number) => `${pad2(sec / 60)}:${pad2(sec % 60)}`;

/** 落盘格式：每行 `[mm:ss] 内容`。带上时间戳才能还原成气泡。 */
function serialize(jots: Jot[]): string {
  return jots.map((j) => `[${stamp(j.at)}] ${j.text}`).join(NL);
}

function parse(raw: string): Jot[] {
  return raw
    .split(NL)
    .map((line) => line.trim())
    .filter((line) => line !== "")
    .map((line) => {
      const m = /^\[(\d{1,2}):(\d{2})\]\s*(.*)$/.exec(line);
      if (!m) return { at: 0, text: line };
      return { at: Number(m[1]) * 60 + Number(m[2]), text: m[3] };
    });
}

/**
 * 一场会议的现场：起名 → 录制 → 随手记 → 结束。
 *
 * 主动作只有「开始录制」一个；导入音频是次要入口，压在下面一行小字里，
 * 不跟录制抢视觉重心。结束后不暴露「排队」「转写」这些内部阶段，
 * 用户需要知道的只有「在整理」以及大概到哪儿了。
 */
export default function Session({
  liveId, liveTitle, liveElapsed, busyId, audioExts,
  onStarted, onEnded, onImport, onOpenMeeting, onError,
}: Props) {
  const [title, setTitle] = useState("");
  const [jots, setJots] = useState<Jot[]>([]);
  const [draft, setDraft] = useState("");
  const [step, setStep] = useState<ProcessEvent | null>(null);
  /** 转写阶段的细粒度进度，来自引擎自己的 stage 事件。 */
  const [asr, setAsr] = useState<number | null>(null);
  /** 录制中的告警（目前只有系统轨静音）。后端判定，前端只负责显示。 */
  const [warning, setWarning] = useState<string | null>(null);
  const bottom = useRef<HTMLDivElement>(null);

  /** 每轨的电平历史，最新的在末尾。存 ref 不存 state。 */
  const history = useRef<Record<TrackSource, number[]>>({ Mic: [], System: [] });
  const barsEl = useRef<Record<TrackSource, HTMLDivElement | null>>({
    Mic: null,
    System: null,
  });

  const recording = liveId !== null;
  const processing = busyId !== null;

  useEffect(() => {
    const off: Array<() => void> = [];
    void events.onProcessProgress(setStep).then((f) => off.push(f));
    void events
      .onProcessDone(() => {
        setStep(null);
        setAsr(null);
      })
      .then((f) => off.push(f));
    void events.onTranscribeProgress((e) => setAsr(e.progress)).then((f) => off.push(f));
    void events
      .onRecordingLevel((e) => {
        const h = history.current[e.source];
        if (!h) return;
        h.push(toHeight(e.rms));
        if (h.length > METER_SLOTS) h.splice(0, h.length - METER_SLOTS);
      })
      .then((f) => off.push(f));
    void events.onRecordingWarning((e) => setWarning(e.message)).then((f) => off.push(f));
    return () => off.forEach((f) => f());
  }, []);

  // 录制期间把电平历史刷到 DOM。挂 rAF 而不是 setState：
  // 20Hz × 2 轨 = 每秒 40 次重渲染，够把主线程拖出掉帧。
  useEffect(() => {
    if (!recording) {
      history.current = { Mic: [], System: [] };
      setWarning(null);
      return;
    }
    let raf = 0;
    const paint = () => {
      for (const { key } of TRACKS) {
        const el = barsEl.current[key];
        if (!el) continue;
        const h = history.current[key];
        const spans = el.children;
        // 历史右对齐：最新的一格永远在最右边，左边不够就留空。
        const offset = METER_SLOTS - h.length;
        for (let i = 0; i < spans.length; i++) {
          const v = i < offset ? 0 : h[i - offset];
          (spans[i] as HTMLElement).style.height = `${Math.max(2, v * 100)}%`;
        }
      }
      raf = requestAnimationFrame(paint);
    };
    raf = requestAnimationFrame(paint);
    return () => cancelAnimationFrame(raf);
  }, [recording]);

  useEffect(() => {
    bottom.current?.scrollIntoView({ behavior: "smooth" });
  }, [jots.length]);

  // 从别处切回正在录制的会议时，把已经记下的内容读回来。
  // 组件卸载会丢掉本地 state，笔记的真源始终是数据库。
  useEffect(() => {
    if (liveId === null) {
      setJots([]);
      return;
    }
    let stale = false;
    void api
      .getNote(liveId)
      .then((raw) => {
        if (!stale) setJots(parse(raw));
      })
      .catch((e) => onError(asMessage(e)));
    return () => {
      stale = true;
    };
  }, [liveId, onError]);

  const start = async () => {
    const name = title.trim() || "未命名会议";
    try {
      const id = await api.startRecording(name);
      onStarted(id, name);
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const end = async () => {
    if (liveId === null) return;
    const id = liveId;
    try {
      if (jots.length > 0) {
        await api.saveNote(id, serialize(jots));
      }
      await api.stopRecording();
      onEnded(id);
      await api.processMeeting(id, null);
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const addJot = async () => {
    const text = draft.trim();
    if (text === "" || liveId === null) return;
    const next = [...jots, { at: liveElapsed, text }];
    setJots(next);
    setDraft("");
    // 每记一条就落盘一次——笔记是最不能丢的东西。
    try {
      await api.saveNote(liveId, serialize(next));
    } catch (e) {
      onError(asMessage(e));
    }
  };

  /** 从本机挑一个音频文件导入。拖放之外的第二条路，键盘用户也得有得走。 */
  const pick = async () => {
    try {
      const chosen = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "音频", extensions: audioExts }],
      });
      if (typeof chosen === "string") onImport(chosen);
    } catch (e) {
      onError(asMessage(e));
    }
  };

  const clock = stamp(liveElapsed);

  if (processing) {
    const importing = step?.phase === "importing";
    const detail = importing
      ? step?.detail || "解码"
      : step?.phase === "summarizing"
        ? "撰写纪要"
        : "整理逐字稿";
    // 导入用解码进度，转写用引擎自己的进度；写纪要是流式的，没有百分比可报。
    const raw = importing ? step?.progress ?? 0 : step?.phase === "summarizing" ? null : asr;
    const pct = raw === null || raw === undefined ? null : Math.round(raw * 100);

    return (
      <div className="live-layout">
        <div className="center">
          <div className="wave" aria-hidden>
            {Array.from({ length: BARS }, (_, i) => (
              <span key={i} style={{ animationDelay: `${i * 70}ms` }} />
            ))}
          </div>
          <p style={{ marginTop: 28, fontSize: 15 }}>
            {importing ? "正在解析音频" : "会议已完成，正在整理纪要"}
          </p>
          <p className="data" style={{ marginTop: 4 }}>{detail}</p>
          {pct !== null && (
            <div className="thin-bar">
              <div className="fill" style={{ width: `${pct}%` }} />
            </div>
          )}
          <button
            className="act"
            style={{ marginTop: 36 }}
            onClick={() => busyId !== null && onOpenMeeting(busyId)}
          >
            先去看看这场会议
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="live-layout">
      <div className="center">
        {recording ? (
          <>
            <div className="title-input" style={{ pointerEvents: "none" }}>
              {liveTitle || "未命名会议"}
            </div>
            <div className="meter">
              {TRACKS.map((t) => (
                <TrackMeter
                  key={t.key}
                  label={t.label}
                  silent={t.key === "System" && warning !== null}
                  barsRef={(el) => {
                    barsEl.current[t.key] = el;
                  }}
                />
              ))}
            </div>
            {warning && <p className="meter-warn">{warning}</p>}
            <div className="elapsed">{clock}</div>
            <button className="record stop" onClick={() => void end()}>
              <span className="disc"><i /></span>
              <span className="cap">结束会议</span>
            </button>
          </>
        ) : (
          <>
            <input
              className="title-input"
              value={title}
              placeholder="这场会议叫什么"
              onChange={(e) => setTitle(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && void start()}
            />
            <div className="baseline" />
            <button className="record" onClick={() => void start()}>
              <span className="disc"><i /></span>
              <span className="cap">开始录制</span>
            </button>
            <p className="import-hint">
              已经有录音了？
              <button className="link" onClick={() => void pick()}>选个文件</button>
              ，或者直接把它拖进窗口
            </p>
          </>
        )}
      </div>

      {recording && (
        <div className="notes">
          {jots.length > 0 && (
            <div className="bubbles">
              {jots.map((j, i) => (
                <div className="bubble" key={i}>
                  <span className="data">{stamp(j.at)}</span>
                  {j.text}
                </div>
              ))}
              <div ref={bottom} />
            </div>
          )}
          <label className="note-input">
            <span>记一笔</span>
            <input
              value={draft}
              placeholder="想到什么就写，回车留下"
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && void addJot()}
            />
          </label>
        </div>
      )}
    </div>
  );
}
