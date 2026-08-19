import { FitAddon, Ghostty, Terminal } from "ghostty-web";

const ghostty = Ghostty.load("/ghostty-vt.wasm");

export async function createWebTerminal(container, handlers) {
  const loadedGhostty = await ghostty;
  const blockClipboard = (event) => {
    event.preventDefault();
    event.stopImmediatePropagation();
  };
  const blockLinkActivation = (event) => event.stopImmediatePropagation();
  container.addEventListener("copy", blockClipboard, true);
  container.addEventListener("paste", blockClipboard, true);
  container.addEventListener("contextmenu", blockClipboard, true);
  container.addEventListener("click", blockLinkActivation, true);
  const removeGuards = () => {
    container.removeEventListener("copy", blockClipboard, true);
    container.removeEventListener("paste", blockClipboard, true);
    container.removeEventListener("contextmenu", blockClipboard, true);
    container.removeEventListener("click", blockLinkActivation, true);
  };
  let terminal;
  let fit;
  try {
    terminal = new Terminal({
      ghostty: loadedGhostty,
      cursorBlink: false,
      disableStdin: true,
      fontSize: 13,
      fontFamily: '"JetBrains Mono", "SFMono-Regular", Menlo, Consolas, monospace',
      scrollback: 10_000,
      theme: {
        background: "#11120f",
        foreground: "#e8e6dc",
        cursor: "#d7ff3f",
        selectionBackground: "#44501f",
      },
    });
    fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(container);
    fit.fit();
    fit.observeResize();
    let lastWheelAt = 0;
    terminal.attachCustomWheelEventHandler((event) => {
      const now = performance.now();
      if (event.deltaY === 0 || now - lastWheelAt < 120) return true;
      lastWheelAt = now;
      handlers.onData(event.deltaY < 0 ? "\x1b[5~" : "\x1b[6~");
      return true;
    });
    const dataSubscription = terminal.onData(handlers.onData);
    const resizeSubscription = terminal.onResize(handlers.onResize);
    let disposed = false;
    const dispose = () => {
      if (disposed) return;
      disposed = true;
      dataSubscription.dispose();
      resizeSubscription.dispose();
      fit.dispose();
      terminal.dispose();
      removeGuards();
    };
    return {
      terminal,
      dimensions: () => ({ cols: terminal.cols, rows: terminal.rows }),
      fitDimensions: () => fit.proposeDimensions() || { cols: terminal.cols, rows: terminal.rows },
      focus: () => terminal.focus(),
      reset: () => terminal.reset(),
      resize: (rows, cols) => terminal.resize(cols, rows),
      setWritable: (enabled) => {
        terminal.options.disableStdin = !enabled;
        terminal.options.cursorBlink = enabled;
        container.dataset.mode = enabled ? "live" : "inactive";
      },
      write: (bytes) => terminal.write(bytes),
      dispose,
    };
  } catch (error) {
    fit?.dispose();
    terminal?.dispose();
    removeGuards();
    throw error;
  }
}
