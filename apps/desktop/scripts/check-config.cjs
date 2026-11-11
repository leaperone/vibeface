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
const electronCache = path.join(scratch, 'electron-cache');
fs.mkdirSync(electronCache, { recursive: true });
(async () => {
  try {
    for (const target of ['aarch64-apple-darwin', 'x86_64-pc-windows-msvc',
      'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu']) {
      for (const signed of target.endsWith('-unknown-linux-gnu') ? [''] : ['', '1']) {
        Object.assign(process.env, {
          VIBEFACE_DESKTOP_TARGET: target,
          VIBEFACE_DESKTOP_RUNTIME_STAGE: scratch,
          VIBEFACE_DESKTOP_OUT: path.join(scratch, 'out'),
          VIBEFACE_DESKTOP_ELECTRON_CACHE: electronCache,
          VIBEFACE_DESKTOP_SIGN: signed,
          VIBEFACE_CODESIGN_TEAM: 'TESTTEAM00',
        });
        delete require.cache[require.resolve(configPath)];
        const config = require(configPath);
        if (target === 'aarch64-apple-darwin') {
          assert.match(fs.readFileSync(config.mac.entitlements, 'utf8'),
            /<key>com\.apple\.security\.device\.camera<\/key>\s*<true\s*\/>/,
            'the hardened runtime must allow camera access for join-link scanning');
          assert.equal(config.mac.entitlementsInherit, config.mac.entitlements);
          assert(config.mac.extendInfo.NSCameraUsageDescription.trim(), 'camera access needs a usage description');
        }
        assert.equal(config.electronDownload.cache, electronCache,
          'electron-builder must consume the prefetch cache root');
        await validateConfiguration(config, { isEnabled: false });
      }
    }
    const { buildElectronArtifactConfig } = fromBuilder('app-builder-lib/out/util/electronGet.js');
    const pins = require('../build/electron-checksums.json');
    const downloadConfig = await buildElectronArtifactConfig({
      electronDownload: { isVerifyChecksum: true, checksums: pins.files, cache: electronCache },
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
      force: true, cacheRoot: electronCache, tempDirectory: scratch,
      isVerifyChecksum: true, checksums: { [fileName]: expected }, downloader };
    const verified = await downloadArtifact(options);
    assert.deepEqual(fs.readFileSync(verified), payload);
    assert.equal(calls.length, 1, 'only the archive download is needed');
    bytes = Buffer.from('tampered-archive');
    await assert.rejects(downloadArtifact(options), /checksum|sha256/i);

    const {
      prefetchElectronArchive,
      isTransientDownloadError,
      PREFETCH_ATTEMPTS,
      PREFETCH_BACKOFF_MS,
    } = await import('./prefetch-electron.mjs');
    assert(isTransientDownloadError({ response: { statusCode: 504 } }));
    assert(!isTransientDownloadError({ response: { statusCode: 403 } }));

    const cacheRoot = path.join(scratch, 'prefetch-cache');
    fs.mkdirSync(cacheRoot, { recursive: true });
    const fixturePath = path.join(cacheRoot, fileName);

    const transient = status => {
      const err = new Error(`Response code ${status}`);
      err.response = { statusCode: status };
      return err;
    };

    let attempts = 0;
    const sleeps = [];
    const okDownload = async () => {
      attempts += 1;
      if (attempts === 1) throw transient(504);
      fs.writeFileSync(fixturePath, payload);
      return fixturePath;
    };
    await prefetchElectronArchive({
      target: 'aarch64-apple-darwin',
      cacheRoot,
      tempDirectory: scratch,
      downloadArtifact: okDownload,
      sleep: ms => { sleeps.push(ms); },
      logStatus: () => {},
    });
    assert.equal(attempts, 2, 'a transient 504 must retry once then succeed');
    assert.deepEqual(sleeps, [PREFETCH_BACKOFF_MS[0]]);

    attempts = 0;
    await assert.rejects(prefetchElectronArchive({
      target: 'aarch64-apple-darwin',
      cacheRoot,
      tempDirectory: scratch,
      downloadArtifact: async () => {
        attempts += 1;
        throw transient(403);
      },
      sleep: () => {},
      logStatus: () => {},
    }));
    assert.equal(attempts, 1, 'permanent HTTP 403 must not retry');

    attempts = 0;
    await assert.rejects(prefetchElectronArchive({
      target: 'aarch64-apple-darwin',
      cacheRoot,
      tempDirectory: scratch,
      downloadArtifact: options => {
        attempts += 1;
        return downloadArtifact({
          ...options,
          downloader: { async download(_url, destination) { fs.writeFileSync(destination, Buffer.from('tampered')); } },
        });
      },
      sleep: () => {},
      logStatus: () => {},
    }), /checksum|sha256/i);
    assert.equal(attempts, 1, 'digest mismatch must fail immediately without retry');

    attempts = 0;
    sleeps.length = 0;
    await assert.rejects(prefetchElectronArchive({
      target: 'aarch64-apple-darwin',
      cacheRoot,
      tempDirectory: scratch,
      downloadArtifact: async () => {
        attempts += 1;
        throw transient(504);
      },
      sleep: ms => { sleeps.push(ms); },
      logStatus: () => {},
    }));
    assert.equal(attempts, PREFETCH_ATTEMPTS, 'transient failures exhaust bounded attempts');
    assert.deepEqual(sleeps, PREFETCH_BACKOFF_MS);

    console.log('The pinned packager accepts all native targets and verifies Electron downloads against checksum pins');
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
