import { useEffect, useState } from "react";

export interface NavItem {
  id: string;
  label: string;
}

interface Props {
  items: NavItem[];
}

export default function QuickNav({ items }: Props) {
  const [activeId, setActiveId] = useState<string>(items[0]?.id || "");

  useEffect(() => {
    if (items.length === 0) return;

    const observer = new IntersectionObserver(
      (entries) => {
        // 找到当前进入视口比例最高的元素
        const visible = entries.filter((e) => e.isIntersecting);
        if (visible.length > 0) {
          // 优先按顶部位置最近的元素
          visible.sort((a, b) => Math.abs(a.boundingClientRect.top) - Math.abs(b.boundingClientRect.top));
          setActiveId(visible[0].target.id);
        }
      },
      {
        rootMargin: "-20px 0px -60% 0px",
        threshold: [0, 0.2, 0.5, 1],
      }
    );

    items.forEach((item) => {
      const el = document.getElementById(item.id);
      if (el) observer.observe(el);
    });

    return () => observer.disconnect();
  }, [items]);

  const scrollTo = (id: string) => {
    const el = document.getElementById(id);
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "start" });
      setActiveId(id);
    }
  };

  return (
    <nav className="quick-nav" aria-label="快速导航">
      <div className="quick-nav-inner">
        <ul className="quick-nav-list">
          {items.map((item) => {
            const isActive = activeId === item.id;
            return (
              <li key={item.id}>
                <button
                  type="button"
                  className={`quick-nav-link ${isActive ? "active" : ""}`}
                  onClick={() => scrollTo(item.id)}
                >
                  <span className="quick-nav-dot" />
                  <span className="quick-nav-text">{item.label}</span>
                </button>
              </li>
            );
          })}
        </ul>
      </div>
    </nav>
  );
}
