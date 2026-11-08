'use strict';
// Validate both desktop configurations against the schema shipped by the pinned packager.
const { createRequire } = require('node:module');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const fromBuilder = createRequire(require.resolve('electron-builder'));
const { validateConfiguration } = fromBuilder('app-builder-lib/out/util/config/config.js');
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'vibeface-builder-config-'));
const configPath = path.resolve(__dirname, '../electron-builder.cjs');
(async () => {
  try {
    for (const target of ['aarch64-apple-darwin', 'x86_64-pc-windows-msvc',
      'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu']) {
      for (const signed of target.endsWith('-unknown-linux-gnu') ? [''] : ['', '1']) {
        Object.assign(process.env, {
          VIBEFACE_DESKTOP_TARGET: target,
          VIBEFACE_DESKTOP_RUNTIME_STAGE: scratch,
          VIBEFACE_DESKTOP_OUT: path.join(scratch, 'out'),
          VIBEFACE_DESKTOP_SIGN: signed,
          VIBEFACE_CODESIGN_TEAM: 'TESTTEAM00',
        });
        delete require.cache[require.resolve(configPath)];
        await validateConfiguration(require(configPath), { isEnabled: false });
      }
    }
    console.log('The pinned packager accepts all native targets, signed and unsigned where applicable');
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
