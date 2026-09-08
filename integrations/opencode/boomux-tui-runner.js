const MAX_OUTPUT = 64 * 1024;
const COMMAND_TIMEOUT_MS = 5_000;

async function readBounded(stream, limit, readers) {
  if (!stream) return "";
  const reader = stream.getReader();
  readers.add(reader);
  const decoder = new TextDecoder();
  let size = 0;
  let result = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) {
        throw new Error("boomux output limit exceeded");
      }
      result += decoder.decode(value, { stream: true });
    }
    return result + decoder.decode();
  } catch (error) {
    void reader.cancel().catch(() => {});
    throw error;
  } finally {
    readers.delete(reader);
    reader.releaseLock();
  }
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
    const readers = new Set();
    let timer;
    // Killing alone does not settle exited or inherited output pipes.
    const deadline = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("boomux command timed out")), timeoutMs);
    });
    try {
      const [stdout, stderr, exitCode] = await Promise.race([
        Promise.all([
          readBounded(child.stdout, MAX_OUTPUT, readers),
          readBounded(child.stderr, MAX_OUTPUT, readers),
          child.exited,
        ]),
        deadline,
      ]);
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
    } catch (error) {
      try { child.kill?.("SIGKILL"); } catch { /* Already exited. */ }
      throw error;
    } finally {
      clearTimeout(timer);
      for (const reader of readers) {
        // Do not wait on a broken pipe's cancellation to settle the command.
        void reader.cancel().catch(() => {});
      }
    }
  };
}
