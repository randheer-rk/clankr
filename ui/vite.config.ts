import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// `npm run dev` proxies the API to a local clankrd (self-signed cluster CA, hence secure: false).
export default defineConfig({
  plugins: [react()],
  server: { proxy: { '/v1': { target: process.env.CLANKR_SERVER ?? 'https://127.0.0.1:7600', secure: false, changeOrigin: true } } },
  build: { outDir: 'dist', emptyOutDir: true },
})
