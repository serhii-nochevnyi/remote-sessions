import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const deviceHost = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: deviceHost ?? "127.0.0.1",
    port: 1420,
    strictPort: true,
    hmr: deviceHost ? { protocol: "ws", host: deviceHost, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**", "**/target/**"] },
  },
});
