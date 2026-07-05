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
      '/api': { target: 'http://localhost:8099', changeOrigin: true },
    },
  },
})
