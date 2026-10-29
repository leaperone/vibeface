#!/usr/bin/env node
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { crc32, deflateSync } from 'node:zlib';

const repo = fileURLToPath(new URL('../', import.meta.url));
const { chromium } = createRequire(fs.realpathSync(path.join(repo, 'node_modules/@e2e-dev/web')) + '/')('playwright-core');

const GREEN = '#4ADE80';
const glyph = color => `<g fill="none" stroke="${color}" stroke-width="64" stroke-linecap="round" stroke-linejoin="round"><path d="M336 476 L400 404 L464 476"/><path d="M560 476 L624 404 L688 476"/></g><rect x="442" y="580" width="140" height="72" rx="16" fill="${color}"/>`;
const gradient = '<radialGradient id="bg" cx=".5" cy=".45" r=".75"><stop offset="0" stop-color="#11241A"/><stop offset="1" stop-color="#050806"/></radialGradient>';
const glow = '<filter id="glow" filterUnits="userSpaceOnUse" x="0" y="0" width="1024" height="1024"><feGaussianBlur stdDeviation="22" result="b"/><feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>';
const svg = body => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">${body}</svg>\n`;

const glowBackground = '<rect width="1024" height="1024" fill="url(#bg)"/>';
const glowGlyph = `<g filter="url(#glow)">${glyph(GREEN)}</g>`;
const appIcon = svg(`<defs>${gradient}${glow}</defs>${glowBackground}${glowGlyph}`);

const icons = [
  { out: 'apps/mobile/assets/icon/app-icon.svg', svg: appIcon },
  { out: 'crates/node/web/public/icon.svg', svg: svg(`<rect width="1024" height="1024" rx="224" fill="#0B100D"/>${glyph(GREEN)}`) },
  { out: 'apps/mobile/assets/icon/ios-light.png', size: 1024, opaque: true, svg: appIcon },
  { out: 'apps/mobile/assets/icon/ios-tinted.png', size: 1024, opaque: true, svg: svg(`<rect width="1024" height="1024" fill="#000"/>${glyph('#FFF')}`) },
  { out: 'apps/mobile/assets/icon/android-foreground.png', size: 1024, opaque: false, svg: svg(`<defs>${glow}</defs>${glowGlyph}`) },
  { out: 'apps/mobile/assets/icon/android-background.png', size: 1024, opaque: true, svg: svg(`<defs>${gradient}</defs>${glowBackground}`) },
  { out: 'apps/mobile/assets/icon/android-monochrome.png', size: 1024, opaque: false, svg: svg(glyph('#FFF')) },
];

function encodePng(rgba, size, opaque) {
  const channels = opaque ? 3 : 4;
  const rows = Buffer.alloc(size * (1 + size * channels));
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const from = (y * size + x) * 4;
      const to = y * (1 + size * channels) + 1 + x * channels;
      if (opaque) assert.equal(rgba[from + 3], 255, `pixel ${x},${y} must be opaque`);
      rgba.copy(rows, to, from, from + channels);
    }
  }
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type), data]);
    const frame = Buffer.alloc(body.length + 8);
    frame.writeUInt32BE(data.length, 0);
    body.copy(frame, 4);
    frame.writeUInt32BE(crc32(body), body.length + 4);
    return frame;
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header.set([8, opaque ? 2 : 6, 0, 0, 0], 8);
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', header), chunk('IDAT', deflateSync(rows, { level: 9 })), chunk('IEND', Buffer.alloc(0)),
  ]);
}

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  for (const icon of icons) {
    const out = path.join(repo, icon.out);
    fs.mkdirSync(path.dirname(out), { recursive: true });
    if (!icon.size) {
      fs.writeFileSync(out, icon.svg);
      continue;
    }
    const pixels = await page.evaluate(async ({ markup, size }) => {
      const image = new Image();
      image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(markup)}`;
      await image.decode();
      const canvas = new OffscreenCanvas(size, size);
      const context = canvas.getContext('2d', { colorSpace: 'srgb' });
      context.drawImage(image, 0, 0, size, size);
      const bytes = context.getImageData(0, 0, size, size).data;
      let binary = '';
      for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
      return btoa(binary);
    }, { markup: icon.svg, size: icon.size });
    fs.writeFileSync(out, encodePng(Buffer.from(pixels, 'base64'), icon.size, icon.opaque));
  }
} finally {
  await browser.close();
}
console.log(icons.map(icon => icon.out).join('\n'));
