const themeButton = document.querySelector<HTMLButtonElement>("#theme-toggle");
const root = document.documentElement;
function updateTheme(theme: string) {
  root.dataset.theme = theme;
  themeButton?.setAttribute(
    "aria-label",
    theme === "dark" ? "Use light theme" : "Use dark theme",
  );
  themeButton?.setAttribute("aria-pressed", String(theme === "light"));
  document
    .querySelector('meta[name="theme-color"]')
    ?.setAttribute("content", theme === "dark" ? "#151620" : "#f5f3ed");
}
updateTheme(root.dataset.theme || "dark");
if (themeButton) {
  themeButton.hidden = false;
  themeButton.addEventListener("click", () => {
    const theme = root.dataset.theme === "dark" ? "light" : "dark";
    updateTheme(theme);
    try {
      localStorage.setItem("boomux-site-theme", theme);
    } catch {}
  });
}

const tabs = [
  ...document.querySelectorAll<HTMLButtonElement>("[data-install]"),
];
function selectInstall(selected: HTMLButtonElement) {
  for (const tab of tabs) {
    const active = tab === selected;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
    const panel = document.getElementById(tab.getAttribute("aria-controls")!);
    if (panel) panel.hidden = !active;
  }
  document.getElementById("copy-status")!.textContent = "";
}
const tablist = document.getElementById("install-tabs");
if (tablist && tabs.length) {
  tablist.hidden = false;
  document.querySelectorAll<HTMLElement>("[data-panel]").forEach((panel) => {
    panel.setAttribute("role", "tabpanel");
    panel.setAttribute("aria-labelledby", `tab-${panel.dataset.panel}`);
    panel.tabIndex = 0;
  });
  selectInstall(tabs[0]);
  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => selectInstall(tab));
    tab.addEventListener("keydown", (event) => {
      const next =
        event.key === "ArrowRight"
          ? (index + 1) % tabs.length
          : event.key === "ArrowLeft"
            ? (index + tabs.length - 1) % tabs.length
            : event.key === "Home"
              ? 0
              : event.key === "End"
                ? tabs.length - 1
                : -1;
      if (next < 0) return;
      event.preventDefault();
      selectInstall(tabs[next]);
      tabs[next].focus();
    });
  });
}
document
  .querySelectorAll<HTMLButtonElement>("[data-copy]")
  .forEach((button) => {
    button.hidden = false;
    button.addEventListener("click", async () => {
      const status = document.getElementById("copy-status");
      const command =
        document.getElementById(button.dataset.copy!)?.textContent || "";
      try {
        await navigator.clipboard.writeText(command);
        if (status)
          status.textContent = "Copied. Paste into your terminal to install.";
      } catch {
        if (status)
          status.textContent =
            "Clipboard unavailable. Select and copy the command above.";
      }
    });
  });
