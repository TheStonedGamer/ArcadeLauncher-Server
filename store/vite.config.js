import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The store is served at the domain root (arcade.orlandoaio.net). During `npm run
// dev`, API calls to /api/* are proxied to the live k3s NodePort so you develop
// against real catalog data without CORS. In production, nginx serves these built
// assets and proxies /api itself, so no proxy config ships in the bundle.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': { target: 'http://10.0.0.112:30721', changeOrigin: true },
      '/art': { target: 'http://10.0.0.112:30721', changeOrigin: true },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: false,
  },
})
