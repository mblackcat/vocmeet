import { useEffect, useId, useRef, useState } from "react";
import { marked } from "marked";
import mermaid from "mermaid";

// 初始化 mermaid，配置极简稿纸中性主题
mermaid.initialize({
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

interface Props {
  content: string;
  className?: string;
}

export default function MarkdownRenderer({ content, className = "markdown-body" }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const baseId = useId().replace(/:/g, "_");
  const [renderedHtml, setRenderedHtml] = useState("");

  useEffect(() => {
    // 自定义 renderer 截获 mermaid 代码块
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
      const html = marked.parse(content) as string;
      setRenderedHtml(html);
    } catch {
      setRenderedHtml(`<pre>${escapeHtml(content)}</pre>`);
    }
  }, [content, baseId]);

  useEffect(() => {
    if (!containerRef.current) return;
    const wraps = containerRef.current.querySelectorAll<HTMLDivElement>(".mermaid-diagram-wrap");

    wraps.forEach(async (wrap, i) => {
      const rawCode = decodeURIComponent(wrap.dataset.mermaid || "");
      if (!rawCode) return;
      const graphId = `mermaid_svg_${baseId}_${i}`;

      try {
        const { svg } = await mermaid.render(graphId, rawCode);
        wrap.innerHTML = svg;
      } catch (err) {
        wrap.innerHTML = `<div class="mermaid-error"><span class="data">脑图渲染失败</span><pre>${escapeHtml(rawCode)}</pre></div>`;
      }
    });
  }, [renderedHtml, baseId]);

  return (
    <div
      ref={containerRef}
      className={`prose ${className}`}
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
