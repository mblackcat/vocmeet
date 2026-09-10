import { useEffect, useRef, useState } from "react";

interface Props {
  attendees: string[];
}

export default function AttendeesFold({ attendees }: Props) {
  const [expanded, setExpanded] = useState(false);
  const [needsFold, setNeedsFold] = useState(false);
  const textRef = useRef<HTMLParagraphElement>(null);

  useEffect(() => {
    const el = textRef.current;
    if (!el) return;
    // 每行约为 24px 高度，3 行约为 72px
    // 如果实际滚动高度大于 76px，说明超过 3 行需要折叠
    if (el.scrollHeight > 76) {
      setNeedsFold(true);
    } else {
      setNeedsFold(false);
    }
  }, [attendees]);

  if (!attendees || attendees.length === 0) return null;

  return (
    <div className={`attendees-wrapper ${expanded ? "expanded" : "folded"}`}>
      <div
        className={`attendees-box ${needsFold && !expanded ? "clamped" : ""}`}
        onClick={() => {
          if (needsFold && !expanded) {
            setExpanded(true);
          }
        }}
        title={needsFold && !expanded ? `点击查看完整参会人列表（共 ${attendees.length} 人）` : undefined}
      >
        <p className="people" ref={textRef}>
          参会 <b>{attendees.join("、")}</b>
        </p>

        {needsFold && !expanded && (
          <div className="attendees-mask">
            <span className="attendees-tip">点击查看完整信息 ({attendees.length} 人)</span>
          </div>
        )}
      </div>

      {needsFold && expanded && (
        <div className="attendees-collapse-row">
          <button
            type="button"
            className="attendees-collapse-btn"
            onClick={() => setExpanded(false)}
          >
            收起 ↑
          </button>
        </div>
      )}
    </div>
  );
}
