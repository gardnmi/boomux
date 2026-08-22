import "@fontsource/jetbrains-mono/latin-400.css";
import { FitAddon, Ghostty, Terminal } from "ghostty-web";

let initialization;
const encoder = new TextEncoder();

async function loadGhostty() {
  const response = await fetch("/ghostty-vt.wasm");
  if (!response.ok) throw new Error(`Ghostty WASM failed to load (${response.status})`);
  const module = await WebAssembly.compile(await response.arrayBuffer());
  const instance = await WebAssembly.instantiate(module, { env: { log() {} } });
  return new Ghostty(instance);
}

export async function createTerminal(container, callbacks) {
  initialization ||= Promise.all([
    loadGhostty(),
    document.fonts?.load('12px "JetBrains Mono"').catch(() => []) || Promise.resolve(),
  ]);
  const [ghostty] = await initialization;
  const terminal = new Terminal({
    ghostty,
    allowProposedApi: false,
    cursorBlink: false,
    cursorStyle: "block",
    fontFamily: '"JetBrains Mono", "JetBrainsMono Nerd Font", monospace',
    fontSize: 12,
    scrollback: 10_000,
    theme: {
      background: "#1e1e2e",
      foreground: "#cdd6f4",
      cursor: "#bac2de",
      cursorAccent: "#1e1e2e",
      selectionBackground: "#f5e0dc",
      selectionForeground: "#bac2de",
      black: "#1e1e2e",
      red: "#f38ba8",
      green: "#a6e3a1",
      yellow: "#f9e2af",
      blue: "#89b4fa",
      magenta: "#f5c2e7",
      cyan: "#94e2d5",
      white: "#cdd6f4",
      brightBlack: "#45475a",
      brightRed: "#f38ba8",
      brightGreen: "#a6e3a1",
      brightYellow: "#f9e2af",
      brightBlue: "#89b4fa",
      brightMagenta: "#f5c2e7",
      brightCyan: "#94e2d5",
      brightWhite: "#bac2de",
    },
  });
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminal.open(container);
  const textarea = terminal.textarea;
  let dimensionsLocked = false;
  let composing = false;
  const handleCompositionStart = () => { composing = true; };
  const handleCompositionEnd = () => {
    composing = false;
    if (textarea) textarea.value = "";
  };
  const handleInput = () => {
    if (!textarea || composing || !textarea.value) return;
    terminal.input(textarea.value.replace(/\r\n|\n/g, "\r"), true);
    textarea.value = "";
  };
  const handleKeyDown = (event) => {
    if (event.defaultPrevented || event.key !== "Enter") return;
    event.preventDefault();
    terminal.input("\r", true);
  };
  if (textarea) {
    textarea.enterKeyHint = "send";
    textarea.addEventListener("compositionstart", handleCompositionStart);
    textarea.addEventListener("compositionend", handleCompositionEnd);
    textarea.addEventListener("input", handleInput);
  }
  container.addEventListener("keydown", handleKeyDown);

  const dimensions = () => ({
    rows: terminal.rows,
    cols: terminal.cols,
    pixel_width: Math.min(65_535, Math.round(container.clientWidth)),
    pixel_height: Math.min(65_535, Math.round(container.clientHeight)),
  });
  fit.observeResize();
  const dataDisposable = terminal.onData((data) => callbacks.input(encoder.encode(data)));
  const resizeDisposable = terminal.onResize(() => {
    if (!dimensionsLocked) callbacks.resize(dimensions());
  });
  requestAnimationFrame(() => {
    fit.fit();
    callbacks.resize(dimensions());
  });

  return {
    dimensions,
    resize: ({ rows, cols }) => {
      dimensionsLocked = true;
      fit.dispose();
      terminal.resize(cols, rows);
    },
    focus: () => textarea?.focus() || terminal.focus(),
    input: (data) => terminal.input(data, true),
    write: (data) => terminal.write(data),
    dispose: () => {
      textarea?.removeEventListener("compositionstart", handleCompositionStart);
      textarea?.removeEventListener("compositionend", handleCompositionEnd);
      textarea?.removeEventListener("input", handleInput);
      container.removeEventListener("keydown", handleKeyDown);
      dataDisposable.dispose();
      resizeDisposable.dispose();
      terminal.dispose();
    },
  };
}
