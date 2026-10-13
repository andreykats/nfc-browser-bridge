import { defineConfig } from "vitest/config";

// GitHub Pages serves the site under /<repo>/; CI passes BASE=/<repo>/.
export default defineConfig({
  base: process.env.BASE ?? "/topps-nfc-bridge/",
  server: { fs: { allow: [".."] } },
  test: { include: ["test/**/*.test.ts"] },
});
