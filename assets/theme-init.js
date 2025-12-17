(() => {
  const storageKey = "dossier-site-theme";
  const systemQuery = window.matchMedia("(prefers-color-scheme: light)");

  const safeGet = () => {
    try {
      return typeof localStorage !== "undefined"
        ? localStorage.getItem(storageKey)
        : null;
    } catch {
      return null;
    }
  };

  const safeSet = (value) => {
    try {
      if (typeof localStorage !== "undefined")
        localStorage.setItem(storageKey, value);
    } catch {
      /* ignore */
    }
  };

  const getMode = () => {
    const stored = safeGet();
    return stored === "light" || stored === "dark" || stored === "auto"
      ? stored
      : "auto";
  };

  const resolveTheme = (mode) => {
    if (mode === "light" || mode === "dark") return mode;
    return systemQuery.matches ? "light" : "dark";
  };

  const applyMode = (mode) => {
    const resolved = resolveTheme(mode);
    document.documentElement.dataset.theme = resolved;
    document.documentElement.dataset.themeMode = mode;
    safeSet(mode);
  };

  systemQuery.addEventListener("change", () => {
    if (getMode() === "auto") applyMode("auto");
  });

  applyMode(getMode());

  window.__themeControls = {
    getMode,
    applyMode,
    modes: ["light", "dark", "auto"],
  };
})();
