#!/usr/bin/env node
import * as esbuild from 'esbuild'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(fileURLToPath(import.meta.url))
const desktopRoot = join(root, '..')

const distMain = join(desktopRoot, 'dist/main')
const distPreload = join(desktopRoot, 'dist/preload')
mkdirSync(distMain, { recursive: true })
mkdirSync(distPreload, { recursive: true })

const common = {
  bundle: true,
  platform: 'node',
  target: 'node20',
  sourcemap: true,
  logLevel: 'info',
}

await esbuild.build({
  ...common,
  entryPoints: [join(desktopRoot, 'src/main/index.ts')],
  outfile: join(distMain, 'index.cjs'),
  format: 'cjs',
  external: ['electron'],
})

await esbuild.build({
  ...common,
  entryPoints: [join(desktopRoot, 'src/preload/launcher.ts')],
  outfile: join(distPreload, 'launcher.cjs'),
  format: 'cjs',
  external: ['electron'],
})

await esbuild.build({
  ...common,
  entryPoints: [join(desktopRoot, 'src/preload/business.ts')],
  outfile: join(distPreload, 'business.cjs'),
  format: 'cjs',
  external: ['electron'],
})

console.log('desktop bundle: dist/main/index.cjs, dist/preload/*.cjs')
