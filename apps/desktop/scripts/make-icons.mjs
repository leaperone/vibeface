#!/usr/bin/env node
/**
 * Regenerates the committed desktop icons from the existing Vibeface brand vectors:
 *   apps/app/public/icon.svg      -> build/icon.icns, build/icon.ico, assets/tray{,@2x}.png
 *   apps/desktop/assets/mark-glyph.svg -> assets/trayTemplate{,@2x}.png (macOS menu bar)
 *
 * Rasterizing uses the macOS `sips` and `iconutil` already on the machine, so this is a
 * macOS-only maintenance step; its outputs are committed and the normal build never runs it.
 */
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const desktopRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const repoRoot = join(desktopRoot, '..', '..')
const appIcon = join(repoRoot, 'apps/app/public/icon.svg')
const glyph = join(desktopRoot, 'assets/mark-glyph.svg')
const buildDir = join(desktopRoot, 'build')
const assetsDir = join(desktopRoot, 'assets')

const work = mkdtempSync(join(tmpdir(), 'vibeface-icons-'))

function render(source, size, outfile) {
  execFileSync('sips', ['-s', 'format', 'png', '-z', String(size), String(size), source, '--out', outfile], {
    stdio: 'ignore',
  })
  return outfile
}

/** ICO with PNG payloads: the format allows them and every target Windows release reads them. */
function writeIco(sizes, outfile) {
  const images = sizes.map((size) => ({
    size,
    bytes: readFileSync(render(appIcon, size, join(work, `ico-${size}.png`))),
  }))
  const header = Buffer.alloc(6)
  header.writeUInt16LE(0, 0)
  header.writeUInt16LE(1, 2)
  header.writeUInt16LE(images.length, 4)
  const directory = Buffer.alloc(16 * images.length)
  let offset = header.length + directory.length
  images.forEach((image, index) => {
    const entry = index * 16
    directory.writeUInt8(image.size >= 256 ? 0 : image.size, entry)
    directory.writeUInt8(image.size >= 256 ? 0 : image.size, entry + 1)
    directory.writeUInt8(0, entry + 2)
    directory.writeUInt8(0, entry + 3)
    directory.writeUInt16LE(1, entry + 4)
    directory.writeUInt16LE(32, entry + 6)
    directory.writeUInt32LE(image.bytes.length, entry + 8)
    directory.writeUInt32LE(offset, entry + 12)
    offset += image.bytes.length
  })
  writeFileSync(outfile, Buffer.concat([header, directory, ...images.map((image) => image.bytes)]))
}

function writeIcns(outfile) {
  const iconset = join(work, 'icon.iconset')
  mkdirSync(iconset, { recursive: true })
  for (const [name, size] of [
    ['icon_16x16.png', 16],
    ['icon_16x16@2x.png', 32],
    ['icon_32x32.png', 32],
    ['icon_32x32@2x.png', 64],
    ['icon_128x128.png', 128],
    ['icon_128x128@2x.png', 256],
    ['icon_256x256.png', 256],
    ['icon_256x256@2x.png', 512],
    ['icon_512x512.png', 512],
    ['icon_512x512@2x.png', 1024],
  ]) {
    render(appIcon, size, join(iconset, name))
  }
  execFileSync('iconutil', ['-c', 'icns', iconset, '-o', outfile], { stdio: 'inherit' })
}

mkdirSync(buildDir, { recursive: true })
writeIcns(join(buildDir, 'icon.icns'))
writeIco([16, 24, 32, 48, 64, 128, 256], join(buildDir, 'icon.ico'))
render(appIcon, 16, join(assetsDir, 'tray.png'))
render(appIcon, 32, join(assetsDir, 'tray@2x.png'))
render(glyph, 16, join(assetsDir, 'trayTemplate.png'))
render(glyph, 32, join(assetsDir, 'trayTemplate@2x.png'))

rmSync(work, { recursive: true, force: true })
console.log('desktop icons: build/icon.icns, build/icon.ico, assets/tray*.png')
