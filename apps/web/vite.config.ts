import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Port + daemon target are injected by start.sh from the shared port
// registry (slot 1 = UI, slot 0 = daemon). Falls back to the historical
// dev defaults when run bare (e.g. `pnpm dev` without the start script).
const uiPort = Number(process.env.CK_UI_PORT) || 5173;
const daemonUrl = process.env.CK_DAEMON_URL || 'http://127.0.0.1:7421';

export default defineConfig({
  plugins: [react()],
  server: {
    // Bind to all interfaces so the dev server is reachable from other
    // devices on the LAN (e.g. phone on the same Wi-Fi). The daemon
    // itself stays on 127.0.0.1; Vite proxies /v1 → daemon locally.
    host: true,
    port: uiPort,
    strictPort: true,
    proxy: {
      // Forward all /v1 calls to the local daemon. Both REST and WS.
      '/v1': {
        target: daemonUrl,
        changeOrigin: false,
        ws: true,
      },
    },
  },
});
