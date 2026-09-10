import { useEffect, useRef } from "react";
import type { Meeting } from "./types";

interface Props {
  x: number;
  y: number;
  meeting: Meeting;
  onClose: () => void;
  onShare: (meeting: Meeting) => void;
  onExportAudio: (meeting: Meeting) => void;
  onExportTranscript: (meeting: Meeting) => void;
  onExportSummary: (meeting: Meeting) => void;
  onArchive: (meeting: Meeting) => void;
}

export default function MeetingContextMenu({
  x,
  y,
  meeting,
  onClose,
  onShare,
  onExportAudio,
  onExportTranscript,
  onExportSummary,
  onArchive,
}: Props) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        onClose();
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", handleDown);
    window.addEventListener("keydown", handleKey);
    return () => {
      window.removeEventListener("mousedown", handleDown);
      window.removeEventListener("keydown", handleKey);
    };
  }, [onClose]);

  // 防止菜单超出窗口边界
  const menuWidth = 190;
  const menuHeight = 220;
  const clampedX = Math.min(x, window.innerWidth - menuWidth - 8);
  const clampedY = Math.min(y, window.innerHeight - menuHeight - 8);

  return (
    <div
      ref={ref}
      className="ctx-menu"
      style={{
        position: "fixed",
        top: Math.max(8, clampedY),
        left: Math.max(8, clampedX),
        zIndex: 100,
      }}
      onClick={(e) => e.stopPropagation()}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div className="ctx-head">
        <span className="ctx-title" title={meeting.title}>
          {meeting.title}
        </span>
      </div>

      <button
        className="ctx-item"
        onClick={() => {
          onShare(meeting);
          onClose();
        }}
      >
        <span className="ctx-label">分享会议纪要</span>
        <span className="ctx-desc">复制到剪贴板</span>
      </button>

      <div className="ctx-divider" />

      <div className="ctx-group-title">导出</div>
      <button
        className="ctx-item"
        onClick={() => {
          onExportAudio(meeting);
          onClose();
        }}
      >
        <span className="ctx-label">音频源 (AAC/录音)</span>
        <span className="ctx-desc">.wav / 音频文件</span>
      </button>
      <button
        className="ctx-item"
        onClick={() => {
          onExportTranscript(meeting);
          onClose();
        }}
      >
        <span className="ctx-label">转出文本 (.md)</span>
        <span className="ctx-desc">纯逐字稿</span>
      </button>
      <button
        className="ctx-item"
        onClick={() => {
          onExportSummary(meeting);
          onClose();
        }}
      >
        <span className="ctx-label">会议纪要 (.md)</span>
        <span className="ctx-desc">结构化纪要</span>
      </button>

      <div className="ctx-divider" />

      <button
        className="ctx-item ctx-danger"
        onClick={() => {
          onArchive(meeting);
          onClose();
        }}
      >
        <span className="ctx-label">归档会议</span>
        <span className="ctx-desc">软删除，可在设置找回</span>
      </button>
    </div>
  );
}
