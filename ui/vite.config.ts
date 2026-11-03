import { execSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { defineConfig, type Plugin } from "vitest/config";
import react from "@vitejs/plugin-react";

function emitBuildInfoPlugin(): Plugin {
  let outDir = "dist";
  return {
    name: "ferrodeck-emit-build-info",
    configResolved(config) {
      outDir = config.build.outDir || "dist";
    },
    closeBundle() {
      let commit = "000000000000";
      let dirty = false;
      try {
        const rawCommit = execSync("git rev-parse --short=12 HEAD", {
          stdio: ["ignore", "pipe", "ignore"],
        })
          .toString()
          .trim()
          .toLowerCase();
        if (/^[0-9a-f]{7,64}$/.test(rawCommit)) {
          commit = rawCommit;
        }
        const statusOut = execSync("git status --porcelain", {
          stdio: ["ignore", "pipe", "ignore"],
        })
          .toString()
          .trim();
        dirty = statusOut.length > 0;
      } catch {
        // Keep fallback commit and dirty=false when git metadata is unavailable
      }

      let version = "0.1.0";
      try {
        const pkgRaw = readFileSync(resolve(process.cwd(), "package.json"), "utf8");
        const pkg = JSON.parse(pkgRaw) as { version?: string };
        if (pkg.version && typeof pkg.version === "string") {
          version = pkg.version;
        }
      } catch {
        // Keep default version
      }

      const payload = {
        commit,
        build_time_unix: Math.floor(Date.now() / 1000),
        version,
        dirty,
      };
      const targetDir = resolve(process.cwd(), outDir);
      mkdirSync(targetDir, { recursive: true });
      writeFileSync(
        resolve(targetDir, "build-info.json"),
        `${JSON.stringify(payload, null, 2)}\n`,
        "utf8",
      );
    },
  };
}

export default defineConfig({
  plugins: [react(), emitBuildInfoPlugin()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8420",
        changeOrigin: true,
      },
      "/ws": {
        target: "ws://127.0.0.1:8420",
        ws: true,
      },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/__tests__/setup.ts"],
  },
});
