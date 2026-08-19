import { mkdir } from "node:fs/promises";

const result = await Bun.build({
  entrypoints: ["terminal-source.js"],
  outdir: ".",
  naming: "terminal.js",
  target: "browser",
  format: "esm",
  minify: true,
});
if (!result.success) {
  for (const message of result.logs) console.error(message);
  process.exit(1);
}

await Bun.write(
  "ghostty-vt.wasm",
  Bun.file("node_modules/ghostty-web/ghostty-vt.wasm"),
);
await mkdir("../licenses", { recursive: true });
await Bun.write(
  "../licenses/ghostty-web-MIT.txt",
  Bun.file("node_modules/ghostty-web/LICENSE"),
);
