const tabs = [...document.querySelectorAll<HTMLButtonElement>("[data-motion-tab]")];
const panels = [...document.querySelectorAll<HTMLElement>("[data-motion-panel]")];
const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
let selected = 0;
let attemptedAutoplay = false;

function pauseAll() {
  panels.forEach(panel => panel.querySelector("video")?.pause());
}

function playSelected() {
  if (motion.matches || document.hidden) return;
  const video = panels[selected]?.querySelector("video");
  if (video) {
    video.muted = true;
    // Autoplay may be blocked; native controls remain available.
    void video.play().catch(() => {});
  }
}

function select(index: number, play = false) {
  pauseAll();
  selected = index;
  tabs.forEach((tab, i) => {
    tab.setAttribute("aria-selected", String(i === index));
    tab.tabIndex = i === index ? 0 : -1;
    panels[i].hidden = i !== index;
  });
  if (play) {
    attemptedAutoplay = true;
    playSelected();
  }
}

if (tabs.length && tabs.length === panels.length) {
  document.querySelector<HTMLElement>(".motion-tabs")!.hidden = false;
  panels.forEach((panel, i) => {
    panel.setAttribute("role", "tabpanel");
    panel.setAttribute("aria-labelledby", tabs[i].id);
  });
  select(0);
  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => select(index, true));
    tab.addEventListener("keydown", event => {
      const next = event.key === "ArrowRight" ? (index + 1) % tabs.length
        : event.key === "ArrowLeft" ? (index + tabs.length - 1) % tabs.length
        : event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1 : -1;
      if (next < 0) return;
      event.preventDefault();
      select(next, true);
      tabs[next].focus();
    });
  });
  const observer = new IntersectionObserver(entries => {
    for (const entry of entries) {
      if (!entry.isIntersecting) pauseAll();
      else if (!attemptedAutoplay) {
        attemptedAutoplay = true;
        playSelected();
      }
    }
  });
  observer.observe(document.querySelector(".motion-showcase")!);
  // Never restart a clip the visitor paused, including when returning to the tab.
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) pauseAll();
  });
  motion.addEventListener("change", () => {
    if (motion.matches) pauseAll();
  });
}
