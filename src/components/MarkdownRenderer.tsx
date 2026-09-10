import { useEffect, useId, useRef, useState } from "react";
import { marked } from "marked";
import type mermaid from "mermaid";

type MermaidApi = typeof mermaid;
let mermaidPromise: Promise<MermaidApi> | null = null;

function getMermaid(): Promise<MermaidApi> {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then((m) => {
      const instance = m.default;
      instance.initialize({
        startOnLoad: false,
        theme: "neutral",
        securityLevel: "loose",
        fontFamily: "inherit",
        mindmap: {
          padding: 16,
          useMaxWidth: true,
        },
        themeVariables: {
          fontFamily: "var(--font-body)",
          primaryColor: "#E6E6E1",
          primaryTextColor: "#141413",
          primaryBorderColor: "#CFCFC8",
          lineColor: "#64645E",
          secondaryColor: "#F2F2EF",
          tertiaryColor: "#FFFFFF",
        },
      });
      return instance;
    });
  }
  return mermaidPromise;
}

interface Props {
  content: string;
  className?: string;
  onJumpToUtterance?: (id: number) => void;
}

/** 智能剥离纪要开头可能重复的标题、时间与参会人信息 */
function stripRedundantHeader(text: string): string {
  const lines = text.split("\n");
  let startIdx = 0;

  while (startIdx < lines.length && lines[startIdx].trim() === "") {
    startIdx++;
  }

  if (startIdx < lines.length && lines[startIdx].trim().startsWith("# ")) {
    startIdx++;
    while (startIdx < lines.length && lines[startIdx].trim() === "") {
      startIdx++;
    }
    if (
      startIdx < lines.length &&
      (lines[startIdx].includes("**时间**") ||
        lines[startIdx].includes("**参会**") ||
        lines[startIdx].includes("时间：") ||
        lines[startIdx].includes("参会："))
    ) {
      startIdx++;
      while (startIdx < lines.length && lines[startIdx].trim() === "") {
        startIdx++;
      }
    }
    return lines.slice(startIdx).join("\n");
  }

  return text;
}

/** 将 [#123] 渲染为可交互的原文溯源胶囊角标 */
function renderCitations(html: string): string {
  return html.replace(
    /\[#(\d+)\]/g,
    '<button type="button" class="cite-pill" data-cite-id="$1" title="溯源：点击查看逐字稿第 $1 条发言原文"><span class="cite-hash">#</span>$1</button>'
  );
}

export default function MarkdownRenderer({
  content,
  className = "markdown-body",
  onJumpToUtterance,
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const baseId = useId().replace(/:/g, "_");
  const [renderedHtml, setRenderedHtml] = useState("");

  useEffect(() => {
    const cleanContent = stripRedundantHeader(content);
    const renderer = new marked.Renderer();
    let mermaidIndex = 0;

    renderer.code = function ({ text, lang }: { text: string; lang?: string }) {
      const isMermaid =
        lang === "mermaid" ||
        lang === "mindmap" ||
        text.trim().startsWith("mindmap") ||
        text.trim().startsWith("graph ") ||
        text.trim().startsWith("flowchart ");

      if (isMermaid) {
        const id = `mermaid_${baseId}_${mermaidIndex++}`;
        const encoded = encodeURIComponent(text);
        return `<div class="mermaid-diagram-wrap" data-mermaid="${encoded}" id="${id}"><div class="mermaid-loading">正在渲染脑图...</div></div>`;
      }
      return `<pre><code class="language-${lang || "text"}">${escapeHtml(text)}</code></pre>`;
    };

    marked.setOptions({
      renderer,
      gfm: true,
      breaks: true,
    });

    try {
      const rawHtml = marked.parse(cleanContent) as string;
      setRenderedHtml(renderCitations(rawHtml));
    } catch {
      setRenderedHtml(`<pre>${escapeHtml(cleanContent)}</pre>`);
    }
  }, [content, baseId]);

  useEffect(() => {
    if (!containerRef.current) return;
    const wraps = containerRef.current.querySelectorAll<HTMLDivElement>(".mermaid-diagram-wrap");
    if (wraps.length === 0) return;

    wraps.forEach(async (wrap, i) => {
      const rawCode = decodeURIComponent(wrap.dataset.mermaid || "");
      if (!rawCode) return;
      const graphId = `mermaid_svg_${baseId}_${i}`;

      try {
        const mermaidApi = await getMermaid();
        const { svg } = await mermaidApi.render(graphId, rawCode);
        wrap.innerHTML = svg;
      } catch {
        wrap.innerHTML = `<div class="mermaid-error"><span class="data">脑图渲染失败</span><pre>${escapeHtml(rawCode)}</pre></div>`;
      }
    });
  }, [renderedHtml, baseId]);

  const handleContainerClick = (e: React.MouseEvent<HTMLDivElement>) => {
    const pill = (e.target as HTMLElement).closest(".cite-pill") as HTMLElement | null;
    if (pill) {
      e.preventDefault();
      e.stopPropagation();
      const citeId = pill.dataset.citeId;
      if (citeId) {
        onJumpToUtterance?.(Number(citeId));
      }
    }
  };

  return (
    <div
      ref={containerRef}
      className={`prose ${className}`}
      onClick={handleContainerClick}
      dangerouslySetInnerHTML={{ __html: renderedHtml }}
    />
  );
}

function escapeHtml(str: string): string {
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}
