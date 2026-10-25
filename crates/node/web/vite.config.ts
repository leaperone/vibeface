import tailwindcss from '@tailwindcss/vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import viteReact from '@vitejs/plugin-react'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'

const cryptoModule = fileURLToPath(new URL('./generated/vibeface_secure.js', import.meta.url))

const config = defineConfig({
  envDir: false,
  resolve: { tsconfigPaths: true },
  plugins: [
    {
      name: 'vibeface-crypto',
      enforce: 'pre',
      resolveId(source, importer) {
        if (source === '/generated/vibeface_secure.js' || source === cryptoModule || (importer && resolve(dirname(importer), source) === cryptoModule)) {
          return { id: this.environment.name === 'client' ? '/generated/vibeface_secure.js' : cryptoModule, external: 'absolute' }
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
