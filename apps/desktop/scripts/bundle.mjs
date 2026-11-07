#!/usr/bin/env node
/**
 * Produces exactly what the delivery build packages:
 *   dist/main/index.cjs       compiled main process (CommonJS, electron external)
 *   dist/preload/launcher.cjs trusted launcher preload (CommonJS, electron external)
 *   dist/launcher/**          launcher renderer: index.html, launcher.js, launcher.css
 * Nothing else is written under dist/.
 */
import * as esbuild from 'esbuild'
import { cp, mkdir, rm } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const desktopRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const dist = join(desktopRoot, 'dist')

await rm(dist, { recursive: true, force: true })
await mkdir(dist, { recursive: true })

const shared = {
  bundle: true,
  sourcemap: true,
  logLevel: 'info',
  legalComments: 'none',
}

const node = {
  ...shared,
  platform: 'node',
  target: 'node22',
  format: 'cjs',
  external: ['electron'],
  // The tray mark travels inside the compiled main so a packaged app needs no loose asset.
  loader: { '.png': 'dataurl' },
}

await esbuild.build({
  ...node,
  entryPoints: [join(desktopRoot, 'src/main/index.ts')],
  outfile: join(dist, 'main/index.cjs'),
})

await esbuild.build({
  ...node,
  entryPoints: [join(desktopRoot, 'src/preload/launcher.ts')],
  outfile: join(dist, 'preload/launcher.cjs'),
})

await esbuild.build({
  ...shared,
  entryPoints: [join(desktopRoot, 'src/launcher/main.ts')],
  outfile: join(dist, 'launcher/launcher.js'),
  platform: 'browser',
  target: 'chrome140',
  format: 'iife',
})

await esbuild.build({
  ...shared,
  entryPoints: [join(desktopRoot, 'src/launcher/launcher.css')],
  outfile: join(dist, 'launcher/launcher.css'),
  target: 'chrome140',
})

await cp(join(desktopRoot, 'src/launcher/index.html'), join(dist, 'launcher/index.html'))

console.log('desktop build: dist/main/index.cjs, dist/preload/launcher.cjs, dist/launcher/index.html')
