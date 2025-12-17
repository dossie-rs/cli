(() => {
  const storageKey = "dossier-site-theme";
  const systemQuery = window.matchMedia("(prefers-color-scheme: light)");
  const modes = ["light", "dark", "auto"];

  const getMode = () => {
    const stored =
      typeof localStorage !== "undefined"
        ? localStorage.getItem(storageKey)
        : null;
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
    if (typeof localStorage !== "undefined")
      localStorage.setItem(storageKey, mode);
  };

  const button = document.getElementById("theme-toggle");
  const icon = button?.querySelector(".theme-icon");
  if (!button || !icon) return;

  const icons = {
    light:
      '<circle cx="12" cy="12" r="5" fill="currentColor" /><path d="M12 2v2.5M12 19.5V22M4.5 12H2M22 12h-2.5M5.8 5.8 4 4M19.9 20 18.2 18.2M5.8 18.2 4 20M19.9 4 18.2 5.8" stroke="currentColor" stroke-width="2" stroke-linecap="round" fill="none" />',
    dark: '<path fill="currentColor" d="M16.8 3.4a8.5 8.5 0 1 0 3.8 16.1 7 7 0 0 1-3.8-16.1Z" />',
    auto: '<path fill="currentColor" d="M12 3a9 9 0 1 0 0 18V3Z" /><path d="M12 4a8 8 0 0 1 0 16" stroke="currentColor" stroke-width="2" stroke-linecap="round" fill="none" />',
  };

  const labels = {
    light: "Light mode",
    dark: "Dark mode",
    auto: "Auto mode (follows system)",
  };

  const updateButton = (mode) => {
    const currentMode = mode ?? getMode();
    icon.innerHTML = icons[currentMode] ?? icons.auto;
    button.dataset.mode = currentMode;
    const label = labels[currentMode] ?? "Switch color theme";
    button.setAttribute("aria-label", `Switch theme (current ${label})`);
    button.setAttribute("title", label);
  };

  button.addEventListener("click", () => {
    const current = getMode();
    const next =
      modes[(modes.indexOf(current) + 1) % modes.length] ?? "auto";
    applyMode(next);
    updateButton(next);
  });

  applyMode(getMode());
  updateButton(getMode());
})();
