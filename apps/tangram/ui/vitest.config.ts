import { defineConfig } from "vitest/config";

// Vitest unit/smoke tests run under jsdom so the editor can mount against a
// real DOM without a browser. The editor-mount smoke test (src/editor.smoke.
// test.ts) guards the CM6 "Config merge conflict" / editor-fails-to-mount
// regression class. This file is intentionally separate from vite.config.ts
// and is NOT in the app tsconfig `include`, so the test-runner types don't
// leak into the production build typecheck (`tsc --noEmit` over `src`).
export default defineConfig({
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
  },
});
