import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Windows 自绘窗口按钮。
 *
 * 系统标题栏已经去掉（和侧栏里的「VocMeet」重复，也不好看）。
 * 关闭不退出：收到托盘，真正退出走托盘右键。
 */
export default function WindowChrome({
  closeHint,
  onDismissHint,
}: {
  closeHint: boolean;
  onDismissHint: () => void;
}) {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    const win = getCurrentWindow();
    let alive = true;
    win.isMaximized().then((v) => {
      if (alive) setMaximized(v);
    });
    const off = win.onResized(() => {
      win.isMaximized().then((v) => {
        if (alive) setMaximized(v);
      });
    });
    return () => {
      alive = false;
      off.then((unlisten) => unlisten());
    };
  }, []);

  const win = () => getCurrentWindow();

  useEffect(() => {
    // 内置双击认的是 Tauri 自己记的最大化状态。无边框窗口上这个状态会和系统错开，
    // 最大化后再双击仍被当成「去最大化」，窗口就还原不了。
    // 捕获阶段先拦住，改成按系统是否已最大化来切换。
    const onDblClick = (e: MouseEvent) => {
      const el = e.target instanceof Element ? e.target : null;
      if (!el?.closest("[data-tauri-drag-region]")) return;
      e.preventDefault();
      e.stopPropagation();
      void invoke("toggle_maximize");
    };
    document.addEventListener("dblclick", onDblClick, true);
    return () => document.removeEventListener("dblclick", onDblClick, true);
  }, []);

  return (
    <div className="win-chrome">
      {closeHint && (
        <div className="close-hint" role="status">
          <p>关闭窗口会收到系统托盘，应用继续在后台运行。</p>
          <p>要退出，在托盘图标上右键，选择「退出 VocMeet」。</p>
          <button type="button" onClick={onDismissHint}>
            知道了
          </button>
        </div>
      )}
      <button
        type="button"
        className="win-btn"
        aria-label="最小化"
        onClick={() => void win().minimize()}
      >
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
          <path d="M1 5.5h8" stroke="currentColor" strokeWidth="1" />
        </svg>
      </button>
      <button
        type="button"
        className="win-btn"
        aria-label={maximized ? "还原" : "最大化"}
        onClick={() => void invoke("toggle_maximize")}
      >
        {maximized ? (
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
            <path
              d="M3 2.5h4.5V7M2.5 3.5h4.5V8h-4.5z"
              fill="none"
              stroke="currentColor"
              strokeWidth="1"
            />
          </svg>
        ) : (
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
            <rect
              x="1.5"
              y="1.5"
              width="7"
              height="7"
              fill="none"
              stroke="currentColor"
              strokeWidth="1"
            />
          </svg>
        )}
      </button>
      <button
        type="button"
        className="win-btn win-btn-close"
        aria-label="关闭到托盘"
        title="收到系统托盘"
        onClick={() => void win().close()}
      >
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
          <path d="M2 2l6 6M8 2L2 8" stroke="currentColor" strokeWidth="1" />
        </svg>
      </button>
    </div>
  );
}
