(() => {
  const initMiniToc = () => {
    const content = document.querySelector(".doc-content");
    const toc = document.querySelector(".mini-toc");
    const list = document.querySelector(".mini-toc__list");
    const layout = toc?.closest(".doc-layout");
    if (!content || !toc || !list) return;

    const slugify = (text) =>
      (text || "")
        .toLowerCase()
        .trim()
        .replace(/[\s\W]+/g, "-")
        .replace(/^-+|-+$/g, "") || "section";

    const headings = Array.from(
      content.querySelectorAll("h2, h3, h4")
    ).filter((node) => (node.textContent || "").trim() !== "");

    if (!headings.length) {
      toc.hidden = true;
      layout?.classList.add("doc-layout--single");
      return;
    }

    toc.hidden = false;
    layout?.classList.remove("doc-layout--single");

    const used = new Set(
      headings
        .map((node) => node.id)
        .filter((id) => id && id.trim() !== "")
    );

    const ensureId = (heading) => {
      const base = slugify(heading.textContent || "section");
      let candidate = heading.id && heading.id.trim() ? heading.id : base;
      let index = 2;
      while (!candidate || used.has(candidate)) {
        candidate = `${base}-${index++}`;
      }
      heading.id = candidate;
      used.add(candidate);
      return candidate;
    };

    list.innerHTML = "";
    const linkMap = new Map();

    headings.forEach((heading) => {
      const level = Math.min(
        4,
        Math.max(2, Number(heading.tagName.slice(1) || 2))
      );
      const id = ensureId(heading);
      const text =
        heading.textContent?.trim() ??
        heading.innerText?.trim() ??
        heading.id;
      const item = document.createElement("li");
      item.className = `mini-toc__item level-${level}`;

      const link = document.createElement("a");
      link.href = `#${id}`;
      link.textContent = text;

      item.appendChild(link);
      list.appendChild(item);
      linkMap.set(id, link);
    });

    let lastClickedId = (() => {
      const hash = window.location.hash?.slice(1) ?? "";
      return hash && hash.trim() !== "" ? hash : null;
    })();

    const activate = (id) => {
      linkMap.forEach((link, key) => {
        link.classList.toggle("active", key === id);
      });
    };

    const isVisible = (el) => {
      const rect = el.getBoundingClientRect();
      const topVisible = rect.top < window.innerHeight * 0.9;
      const bottomVisible = rect.bottom > 80;
      return topVisible && bottomVisible;
    };

    const pickActive = () => {
      if (lastClickedId) {
        const clicked = document.getElementById(lastClickedId);
        if (clicked && isVisible(clicked)) {
          activate(lastClickedId);
          return;
        }
        lastClickedId = null;
      }

      const targetLine = window.innerHeight * 0.35;
      let bestId = headings[0]?.id;
      let bestScore = -Infinity;

      headings.forEach((heading) => {
        if (!isVisible(heading)) return;
        const rect = heading.getBoundingClientRect();
        const score = -Math.abs(rect.top - targetLine);
        if (score > bestScore) {
          bestScore = score;
          bestId = heading.id;
        }
      });

      if (bestId) activate(bestId);
    };

    const observer = new IntersectionObserver(
      () => pickActive(),
      { rootMargin: "-10% 0px -70% 0px", threshold: [0, 1.0] }
    );

    headings.forEach((heading) => observer.observe(heading));
    pickActive();

    list.addEventListener("click", (event) => {
      const target = event.target;
      if (!(target instanceof HTMLElement)) return;
      const link = target.closest("a");
      if (!link || !link.hash) return;
      const id = link.hash.slice(1);
      if (!id) return;
      lastClickedId = id;
      activate(id);
    });

    window.addEventListener("hashchange", () => {
      const id = window.location.hash?.slice(1) ?? "";
      lastClickedId = id && linkMap.has(id) ? id : null;
      pickActive();
    });

    if (lastClickedId && !linkMap.has(lastClickedId)) {
      lastClickedId = null;
    }
    if (lastClickedId) activate(lastClickedId);
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initMiniToc, { once: true });
  } else {
    initMiniToc();
  }
})();
