import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

const shim = (name: string) => fileURLToPath(new URL(`./src/shell/${name}.ts`, import.meta.url));

// `vite build --mode electron`: the same UI for the Electron shell
// (../electron), with @tauri-apps/* swapped for the shell bridge shims.
export default defineConfig(({ mode }) => ({
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: {
    outDir: mode === "electron" ? "dist-electron" : "dist",
    rollupOptions: {
      // The desktop shell plus the phone remote page (Track 11), served by
      // the app's LAN server from the same embedded dist.
      input: {
        main: fileURLToPath(new URL("./index.html", import.meta.url)),
        remote: fileURLToPath(new URL("./remote.html", import.meta.url)),
      },
    },
  },
  // Component tests mount Svelte in jsdom, which needs the client runtime
  // rather than the server build Vitest would otherwise resolve.
  resolve:
    mode === "electron"
      ? {
          alias: {
            "@tauri-apps/api/core": shim("core"),
            "@tauri-apps/api/event": shim("event"),
            "@tauri-apps/api/window": shim("window"),
            "@tauri-apps/plugin-opener": shim("opener"),
          },
        }
      : process.env.VITEST
        ? { conditions: ["browser"] }
        : undefined,
}));
