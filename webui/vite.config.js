import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// Real frontend/backend split (PROJECT.md 4.4 note): this is its own Vite
// project with its own build step, no longer embedded in the kernel binary.
// `npm run dev` proxies /api to the running gateway so the browser sees
// same-origin requests (no CORS setup needed on the kernel side).
export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      // `timeout`/`proxyTimeout: 0` disables the proxy's own idle/duration
      // cutoff — `/api/message` blocks for the agent's *entire* run
      // (kernel/src/gateway.rs `run_trigger`), which for a multi-turn
      // `ssh_exec` chain can genuinely take minutes with no bytes sent in
      // between. Without this, a default proxy timeout can kill that
      // connection client-side while the backend keeps working
      // unaffected — the run finishes and lands in `session.json` same as
      // ever, but the browser that asked for it never finds out, since
      // `sendMessage` was awaiting exactly this connection.
      '/api': { target: 'http://localhost:8099', changeOrigin: true, timeout: 0, proxyTimeout: 0 },
    },
  },
})
