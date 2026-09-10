import { useEffect, useState } from "react";
import { api, asMessage } from "../api";
import type { ParticipantInfo } from "../types";

interface Props {
  onClose: () => void;
  onError: (msg: string) => void;
}

export default function ParticipantModal({ onClose, onError }: Props) {
  const [loading, setLoading] = useState(true);
  const [list, setList] = useState<ParticipantInfo[]>([]);
  const [filter, setFilter] = useState("");

  useEffect(() => {
    let active = true;
    (async () => {
      try {
        const data = await api.listAllParticipants();
        if (active) setList(data);
      } catch (e) {
        if (active) onError(asMessage(e));
      } finally {
        if (active) setLoading(false);
      }
    })();
    return () => {
      active = false;
    };
  }, [onError]);

  const filtered = list.filter((p) => {
    const text = `${p.key} ${p.display_name || ""} ${p.sample}`.toLowerCase();
    return text.includes(filter.toLowerCase().trim());
  });

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal-box participant-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title-group">
            <h3>参会人管理</h3>
            <span className="data">跨会议聚合库</span>
          </div>
          <button className="modal-close" onClick={onClose} aria-label="关闭">
            ×
          </button>
        </div>

        <div className="modal-notice">
          <p className="note" style={{ margin: 0 }}>
            预留能力：后续将支持声线特征匹配（Voiceprint Matching）、跨会议自动归集聚类，以及手动标记姓名批量反哺回会议解析。
          </p>
        </div>

        <div className="participant-toolbar">
          <input
            type="text"
            className="participant-search"
            value={filter}
            placeholder="搜索说话人代号、姓名或样句..."
            onChange={(e) => setFilter(e.target.value)}
          />
          <span className="data">已收录 {list.length} 位说话人</span>
        </div>

        <div className="participant-list">
          {loading ? (
            <div className="hollow" style={{ padding: "40px 0" }}>
              读取参会人数据中...
            </div>
          ) : filtered.length === 0 ? (
            <div className="hollow" style={{ padding: "40px 0" }}>
              {list.length === 0 ? "暂无已识别的参会人数据" : "无匹配的说话人"}
            </div>
          ) : (
            filtered.map((item) => (
              <div className="participant-row" key={item.key}>
                <div className="participant-main">
                  <div className="participant-name-line">
                    <span className="participant-name">
                      {item.display_name || item.key}
                    </span>
                    {item.display_name && (
                      <code className="participant-code">{item.key}</code>
                    )}
                    <span className="chip" style={{ fontSize: 11, padding: "1px 6px" }}>
                      声纹待匹配
                    </span>
                  </div>
                  {item.sample && (
                    <div className="participant-sample" title={item.sample}>
                      “{item.sample}”
                    </div>
                  )}
                </div>

                <div className="participant-meta data">
                  <span>{item.meeting_count} 场会议</span>
                  <span>·</span>
                  <span>{item.utterance_count} 条发言</span>
                  <span>·</span>
                  <span>{item.last_seen ? item.last_seen.slice(0, 10) : ""}</span>
                </div>
              </div>
            ))
          )}
        </div>

        <div className="modal-footer">
          <span className="data">本期已预留管理接口与声学模型扩展槽位</span>
          <button className="act" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
