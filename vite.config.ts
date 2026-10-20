import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Configuração padrão de frontend para Tauri v2
export default defineConfig(async () => ({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
}));
