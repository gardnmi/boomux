import { defineConfig } from "astro/config";

export default defineConfig({
  site: process.env.SITE_URL || "https://gardnmi.github.io",
  base: process.env.BASE_PATH || "/boomux",
  output: "static",
});
