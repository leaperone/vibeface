import tailwindcss from '@tailwindcss/vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import viteReact from '@vitejs/plugin-react'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'

const cryptoModule = fileURLToPath(new URL('./generated/vibeface_secure.js', import.meta.url))

const config = defineConfig({
  resolve: { tsconfigPaths: true },
  plugins: [
    {
      name: 'vibeface-crypto',
      resolveId(source, importer) {
        if (source === '/generated/vibeface_secure.js' || (importer && resolve(dirname(importer), source) === cryptoModule)) {
          return { id: '/generated/vibeface_secure.js', external: true }
        }
      },
    },
    tailwindcss(),
    tanstackStart({
      spa: {
        enabled: true,
      },
    }),
    viteReact(),
  ],
})

export default config
