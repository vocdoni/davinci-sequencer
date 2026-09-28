import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { defineConfig, type Plugin, type ProxyOptions } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import tsconfigPaths from 'vite-tsconfig-paths'

// Dev only: serve public/config.local.json (gitignored) as /config.json when
// it exists, so a developer can point the dev server at another deployment
// without touching the committed defaults.
function localConfig(): Plugin {
  const file = resolve(__dirname, 'public/config.local.json')
  return {
    name: 'davinci-explorer-local-config',
    configureServer(server) {
      server.middlewares.use('/config.json', (_req, res, next) => {
        if (!existsSync(file)) return next()
        res.setHeader('Content-Type', 'application/json')
        res.setHeader('Cache-Control', 'no-store')
        res.end(readFileSync(file))
      })
    },
  }
}

// The same same-origin proxies the nginx image offers, for endpoints without
// CORS: BEACON_URL behind /proxy/beacon/ and each SEQUENCER_URLS entry behind
// /proxy/sequencer/<n>/.
function devProxies(): Record<string, ProxyOptions> {
  const proxies: Record<string, ProxyOptions> = {}
  const add = (prefix: string, target: string) => {
    proxies[prefix] = {
      target: target.replace(/\/+$/, ''),
      changeOrigin: true,
      rewrite: (path) => path.slice(prefix.length) || '/',
    }
  }
  if (process.env.BEACON_URL) add('/proxy/beacon', process.env.BEACON_URL)
  const sequencers = (process.env.SEQUENCER_URLS ?? '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
  sequencers.forEach((url, i) => add(`/proxy/sequencer/${i}`, url))
  return proxies
}

export default defineConfig({
  plugins: [react(), tailwindcss(), tsconfigPaths(), localConfig()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    target: 'es2020',
    rollupOptions: {
      output: {
        // Long-lived vendor chunks: an explorer release rarely changes them.
        manualChunks(id) {
          if (!id.includes('node_modules')) return undefined
          if (/node_modules\/(\.pnpm\/)?(viem|ox|abitype|@noble|@scure|@adraffy)/.test(id)) return 'vendor-chain'
          if (/node_modules\/(\.pnpm\/)?(react|react-dom|react-router|react-router-dom|@remix-run|scheduler)[@/]/.test(id)) return 'vendor-react'
          return 'vendor'
        },
      },
    },
  },
  server: {
    host: '127.0.0.1',
    port: 5173,
    proxy: devProxies(),
  },
  preview: {
    host: '127.0.0.1',
    port: 4173,
  },
})
