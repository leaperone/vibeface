// Bounded, idempotent Electron archive prefetch for scripts/build-desktop.mjs (archive GET only).
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const desktop = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const PINS_PATH = path.join(desktop, 'build/electron-checksums.json');

export const PREFETCH_ATTEMPTS = 5;
export const PREFETCH_BACKOFF_MS = [2000, 4000, 8000, 16000];
const TRANSIENT_HTTP = new Set([429, 500, 502, 503, 504]);
const TRANSIENT_CODES = new Set([
  'ETIMEDOUT', 'ECONNRESET', 'ECONNABORTED', 'EPIPE', 'ENOTFOUND', 'ENETUNREACH', 'EHOSTUNREACH',
]);

/** Classify errors retriable for the main Electron ZIP GET only. */
export function isTransientDownloadError(error) {
  const status = error?.response?.statusCode ?? error?.statusCode;
  if (typeof status === 'number' && TRANSIENT_HTTP.has(status)) return true;
  const code = error?.code;
  if (typeof code === 'string' && TRANSIENT_CODES.has(code)) return true;
  if (error?.name === 'TimeoutError') return true;
  return false;
}

const defaultSleep = ms => new Promise(resolve => setTimeout(resolve, ms));

function resolvePackagerModules() {
  const fromDesktop = createRequire(path.join(desktop, 'package.json'));
  const fromBuilder = createRequire(fromDesktop.resolve('electron-builder'));
  const { buildElectronArtifactConfig } = fromBuilder('app-builder-lib/out/util/electronGet.js');
  const { downloadArtifact } = createRequire(fromBuilder.resolve('app-builder-lib'))('@electron/get');
  return { buildElectronArtifactConfig, downloadArtifact };
}

/** Map a VIBEFACE desktop Rust target to @electron/get coordinates. */
export function electronCoords(target) {
  const table = {
    'aarch64-apple-darwin': { platformName: 'darwin', arch: 'arm64' },
    'x86_64-pc-windows-msvc': { platformName: 'win32', arch: 'x64' },
    'x86_64-unknown-linux-gnu': { platformName: 'linux', arch: 'x64' },
    'aarch64-unknown-linux-gnu': { platformName: 'linux', arch: 'arm64' },
  };
  const coords = table[target];
  assert(coords, `unsupported desktop target for Electron prefetch: ${target}`);
  return coords;
}

function prefetchStatus(error) {
  const status = error?.response?.statusCode ?? error?.statusCode;
  if (typeof status === 'number') return `HTTP ${status}`;
  if (error?.code) return String(error.code);
  return error?.name ?? 'error';
}

/**
 * Download and SHA-verify the pinned Electron archive into cacheRoot before electron-builder runs.
 * Injects downloadArtifact and sleep for offline checks in check-config.cjs.
 */
export async function prefetchElectronArchive({
  target,
  cacheRoot,
  tempDirectory,
  downloadArtifact: downloadArtifactInjected,
  sleep: sleepInjected,
  logStatus = status => console.log(`electron prefetch: ${status}`),
} = {}) {
  assert(target && cacheRoot && path.isAbsolute(cacheRoot), 'target and absolute cacheRoot are required');
  const pins = JSON.parse(fs.readFileSync(PINS_PATH, 'utf8'));
  const { platformName, arch } = electronCoords(target);
  const { buildElectronArtifactConfig, downloadArtifact: resolvedDownload } = resolvePackagerModules();
  const downloadArtifact = downloadArtifactInjected ?? resolvedDownload;
  const sleep = sleepInjected ?? defaultSleep;

  const downloadConfig = await buildElectronArtifactConfig({
    electronDownload: { isVerifyChecksum: true, checksums: pins.files, cache: cacheRoot },
    artifactName: 'electron',
    version: pins.version,
    platformName,
    arch,
  });
  assert.deepEqual(downloadConfig.checksums, pins.files);
  assert.notEqual(downloadConfig.unsafelyDisableChecksums, true);

  const options = {
    ...downloadConfig,
    force: false,
    cacheRoot,
    tempDirectory: tempDirectory ?? cacheRoot,
    isVerifyChecksum: true,
    checksums: pins.files,
  };

  let lastError;
  for (let attempt = 0; attempt < PREFETCH_ATTEMPTS; attempt++) {
    try {
      return await downloadArtifact(options);
    } catch (error) {
      if (!isTransientDownloadError(error)) throw error;
      lastError = error;
      logStatus(`archive GET failed (${prefetchStatus(error)}), attempt ${attempt + 1}/${PREFETCH_ATTEMPTS}`);
      if (attempt >= PREFETCH_ATTEMPTS - 1) break;
      options.force = true;
      await sleep(PREFETCH_BACKOFF_MS[attempt]);
    }
  }
  throw lastError;
}
