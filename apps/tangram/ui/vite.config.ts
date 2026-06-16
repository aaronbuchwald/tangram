import { defineConfig } from "vite";

// The shell is prefix-mounted under `/tangram/` (and could be mounted under a
// tenant namespace), so every asset URL the host serves MUST be relative —
// `base: "./"` makes Vite emit `./assets/...` references instead of absolute
// `/assets/...`. This is the build-time mirror of the app contract's
// "relative fetch paths only" rule (AGENTS.md).
//
// Vitest config lives in `vitest.config.ts` (separate so this build config
// stays free of the test-runner type augmentation and the app `tsconfig`
// — `"types": []` — doesn't have to carry vitest's ambient types).
export default defineConfig({
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Inline nothing as data URLs we can't relativize; keep assets as files.
    assetsInlineLimit: 0,
  },
});
