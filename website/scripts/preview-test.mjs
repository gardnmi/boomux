// Keep the server owned by Playwright, even when Astro's CLI detects an agent
// environment and would otherwise start a detached background process.
import { preview } from "astro";

const server = await preview({ server: { host: "127.0.0.1", port: 4321 } });
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, async () => {
    await server.stop();
    process.exit(0);
  });
}
