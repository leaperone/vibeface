import tailwindcss from '@tailwindcss/vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import viteReact from '@vitejs/plugin-react'
import { fumadocsMdx } from 'fumadocs-mdx/vite'
import { defineConfig } from 'vite'

export default defineConfig({
  envDir: false,
  resolve: { tsconfigPaths: true },
  plugins: [
    fumadocsMdx(),
    tailwindcss(),
    tanstackStart({
      prerender: { enabled: true },
      pages: [{ path: '/api/search', prerender: { outputPath: '/search-index.json', autoSubfolderIndex: false } }],
    }),
    viteReact(),
  ],
})
