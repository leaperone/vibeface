'use strict';
// Validate both desktop configurations against the schema shipped by the pinned packager.
const { createRequire } = require('node:module');
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
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
    const { buildElectronArtifactConfig } = fromBuilder('app-builder-lib/out/util/electronGet.js');
    const pins = require('../build/electron-checksums.json');
    const downloadConfig = await buildElectronArtifactConfig({
      electronDownload: { isVerifyChecksum: true, checksums: pins.files },
      artifactName: 'electron', version: pins.version, platformName: 'darwin', arch: 'arm64',
    });
    assert.deepEqual(downloadConfig.checksums, pins.files, 'the actual packager forwards every official checksum pin');
    assert.notEqual(downloadConfig.unsafelyDisableChecksums, true);
    const { downloadArtifact } = createRequire(fromBuilder.resolve('app-builder-lib'))('@electron/get');
    const payload = Buffer.from('verified-electron-download-fixture');
    const fileName = 'electron-v45.2.3-darwin-arm64.zip';
    const expected = createHash('sha256').update(payload).digest('hex');
    const calls = [];
    let bytes = payload;
    const downloader = { async download(url, destination) {
      calls.push(new URL(url).pathname);
      assert(!url.includes('SHASUMS256'), 'pinned checksums must not need another network request');
      fs.writeFileSync(destination, bytes);
    } };
    const options = { artifactName: 'electron', version: '45.2.3', platform: 'darwin', arch: 'arm64',
      force: true, cacheRoot: path.join(scratch, 'electron-cache'), tempDirectory: scratch,
      isVerifyChecksum: true, checksums: { [fileName]: expected }, downloader };
    const verified = await downloadArtifact(options);
    assert.deepEqual(fs.readFileSync(verified), payload);
    assert.equal(calls.length, 1, 'only the archive download is needed');
    bytes = Buffer.from('tampered-archive');
    await assert.rejects(downloadArtifact(options), /checksum|sha256/i);
    console.log('The pinned packager accepts all native targets and verifies Electron downloads against checksum pins');
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
