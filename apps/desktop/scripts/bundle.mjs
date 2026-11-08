#!/usr/bin/env node
/**
 * Produces exactly what the delivery build packages:
 *   dist/main/index.cjs       compiled main process (CommonJS, electron external)
 *   dist/preload/client.cjs   trusted client preload (CommonJS, electron external)
 * Nothing else is written under dist/.
 */
import * as esbuild from 'esbuild'
import { mkdir, rm } from 'node:fs/promises'
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
  loader: { '.png': 'dataurl' },
}

await esbuild.build({
  ...node,
  entryPoints: [join(desktopRoot, 'src/main/index.ts')],
  outfile: join(dist, 'main/index.cjs'),
})

await esbuild.build({
  ...node,
  entryPoints: [join(desktopRoot, 'src/preload/client.ts')],
  outfile: join(dist, 'preload/client.cjs'),
})

console.log('desktop build: dist/main/index.cjs, dist/preload/client.cjs')
