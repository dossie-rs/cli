(() => {
  const MERMAID_CODE_SELECTOR =
    "pre > code.language-mermaid, pre > code.lang-mermaid, pre > code.mermaid";
  const MERMAID_BLOCK_SELECTOR = "pre.mermaid, div.mermaid";

  const collectMermaidNodes = () => {
    const converted = [];
    document.querySelectorAll(MERMAID_CODE_SELECTOR).forEach((code) => {
      const pre = code.parentElement;
      if (!pre) return;
      const container = document.createElement("pre");
      container.className = "mermaid";
      container.textContent = code.textContent || "";
      pre.replaceWith(container);
      converted.push(container);
    });

    const existing = Array.from(document.querySelectorAll(MERMAID_BLOCK_SELECTOR));
    const combined = [...existing, ...converted];
    const seen = new Set();
    return combined.filter((node) => {
      if (seen.has(node)) return false;
      seen.add(node);
      return true;
    });
  };

  const themeForMermaid = () => {
    const theme = document.documentElement.dataset.theme;
    return theme === "dark" ? "dark" : "default";
  };

  const configureMermaid = () => ({
    startOnLoad: false,
    theme: themeForMermaid(),
    securityLevel: "strict",
    flowchart: { useMaxWidth: true },
    sequence: { useMaxWidth: true },
  });

  const stashSource = (node) => {
    if (!node.dataset.mermaidSource) {
      node.dataset.mermaidSource = node.textContent || "";
    }
  };

  const resetNode = (node) => {
    const source = node.dataset.mermaidSource;
    if (typeof source === "string") {
      node.textContent = source;
    }
    node.removeAttribute("data-processed");
  };

  let queued = false;
  const renderMermaid = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      const nodes = collectMermaidNodes();
      if (!nodes.length) return;

      if (!window.mermaid || typeof window.mermaid.initialize !== "function") {
        console.warn(
          "[dossiers] Mermaid runtime missing. Run scripts/update-mermaid.sh to vendor mermaid.min.js."
        );
        return;
      }

      nodes.forEach(stashSource);
      nodes.forEach(resetNode);

      try {
        window.mermaid.initialize(configureMermaid());
        if (typeof window.mermaid.run === "function") {
          window.mermaid.run({ nodes });
        } else if (typeof window.mermaid.init === "function") {
          window.mermaid.init(undefined, nodes);
        }
      } catch (err) {
        console.warn("[dossiers] Mermaid render failed:", err);
      }
    });
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", renderMermaid, { once: true });
  } else {
    renderMermaid();
  }

  const observer = new MutationObserver((mutations) => {
    if (mutations.some((m) => m.attributeName === "data-theme")) {
      renderMermaid();
    }
  });
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme"],
  });
})();
