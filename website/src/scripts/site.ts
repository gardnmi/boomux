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
