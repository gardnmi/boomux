const MAX_OUTPUT = 64 * 1024;
const COMMAND_TIMEOUT_MS = 5_000;

async function readBounded(stream, limit, onOverflow) {
  if (!stream) return "";
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let size = 0;
  let result = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > limit) {
      onOverflow();
      throw new Error("boomux output limit exceeded");
    }
    result += decoder.decode(value, { stream: true });
  }
  return result + decoder.decode();
}

export function createProcessRunner(options = {}) {
  const spawn = options.spawn ?? globalThis.Bun?.spawn;
  const timeoutMs = options.timeoutMs ?? COMMAND_TIMEOUT_MS;
  if (typeof spawn !== "function") {
    return async () => {
      throw new Error("Bun.spawn is unavailable");
    };
  }

  return async (argv) => {
    const child = spawn(argv, {
      stdin: "ignore",
      stdout: "pipe",
      stderr: "pipe",
      shell: false,
    });
    let timedOut = false;
    const kill = () => child.kill?.();
    const timer = setTimeout(() => {
      timedOut = true;
      kill();
    }, timeoutMs);
    try {
      const [stdout, stderr, exitCode] = await Promise.all([
        readBounded(child.stdout, MAX_OUTPUT, kill),
        readBounded(child.stderr, MAX_OUTPUT, kill),
        child.exited,
      ]);
      if (timedOut) throw new Error("boomux command timed out");
      const value = stdout.trim() ? stdout : stderr;
      if (!value.trim()) throw new Error("boomux returned empty JSON output");
      const result = JSON.parse(value);
      if (exitCode !== 0 || result?.error) {
        const error = new Error(
          result?.error?.message ?? stderr.trim() ?? "boomux command failed",
        );
        error.code = result?.error?.code;
        throw error;
      }
      return result;
    } finally {
      clearTimeout(timer);
    }
  };
}
