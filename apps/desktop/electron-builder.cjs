'use strict';
// electron-builder configuration for the Vibeface desktop host.
// scripts/build-desktop.mjs is the only supported caller: it stages the sealed runtime, pins the
// target and output directory through the environment, and never publishes.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const desktop = __dirname;
const ELECTRON_VERSION = '45.2.3';
const PLATFORMS = {
  'aarch64-apple-darwin': { os: 'mac', arch: 'arm64' },
  'x86_64-pc-windows-msvc': { os: 'win', arch: 'x64' },
};

const target = process.env.VIBEFACE_DESKTOP_TARGET;
const runtime = process.env.VIBEFACE_DESKTOP_RUNTIME_STAGE;
const output = process.env.VIBEFACE_DESKTOP_OUT;
const signed = process.env.VIBEFACE_DESKTOP_SIGN === '1';
assert(PLATFORMS[target], 'VIBEFACE_DESKTOP_TARGET must name a supported desktop platform');
assert(runtime && path.isAbsolute(runtime) && fs.statSync(runtime).isDirectory(), 'VIBEFACE_DESKTOP_RUNTIME_STAGE must be the staged runtime directory');
assert(output && path.isAbsolute(output), 'VIBEFACE_DESKTOP_OUT must be an absolute output directory');
const metadata = JSON.parse(fs.readFileSync(path.join(desktop, 'package.json'), 'utf8'));
assert.equal(metadata.main, 'dist/main/index.cjs', 'the desktop package must point at the compiled main entry');
assert.equal((metadata.devDependencies ?? {}).electron, ELECTRON_VERSION, `the desktop package must pin electron ${ELECTRON_VERSION}`);

assert(!signed || target !== 'aarch64-apple-darwin' || process.env.VIBEFACE_CODESIGN_TEAM,
  'notarizing the macOS application needs the Developer ID team identifier');
const { arch } = PLATFORMS[target];
const artifactName = `vibeface-desktop-\${version}-${target}.\${ext}`;

module.exports = {
  appId: 'one.leaper.vibeface.desktop',
  productName: 'Vibeface',
  copyright: 'LEAPER ONE',
  electronVersion: ELECTRON_VERSION,
  directories: { output, buildResources: path.join(desktop, 'build') },
  // Only the compiled host ships inside the asar; business UI comes from the bundled Core.
  files: ['package.json', 'dist/main/**', 'dist/preload/**', 'dist/launcher/**'],
  // The runtime is already sealed by FILES.sha256, so it is copied verbatim and never rewritten.
  extraResources: [{ from: runtime, to: 'runtime' }],
  asar: true,
  npmRebuild: false,
  buildDependenciesFromSource: false,
  removePackageScripts: true,
  removePackageKeywords: true,
  // A build never publishes; publication stays with the single product release publisher.
  publish: null,
  forceCodeSigning: signed,
  mac: {
    target: [{ target: 'dmg', arch: [arch] }, { target: 'zip', arch: [arch] }],
    category: 'public.app-category.productivity',
    artifactName,
    hardenedRuntime: true,
    gatekeeperAssess: false,
    entitlements: path.join(desktop, 'build/entitlements.mac.plist'),
    entitlementsInherit: path.join(desktop, 'build/entitlements.mac.plist'),
    // Re-signing the application must not touch the already signed and sealed CLI and Core.
    signIgnore: ['Contents/Resources/runtime/'],
    identity: signed ? undefined : null,
    // Notarization uses the existing Developer ID and App Store Connect key, then staples the app.
    notarize: signed ? { teamId: process.env.VIBEFACE_CODESIGN_TEAM } : false,
    extendInfo: { NSHighResolutionCapable: true, LSMinimumSystemVersion: '12.0.0' },
  },
  win: {
    target: [{ target: 'nsis', arch: [arch] }, { target: 'zip', arch: [arch] }],
    artifactName,
    // No Authenticode certificate is configured for development previews; nothing is faked.
    signtoolOptions: signed ? { signingHashAlgorithms: ['sha256'] } : undefined,
  },
  nsis: {
    // A per-user installer: no administrator rights, no WSL, no Developer Mode.
    oneClick: false,
    perMachine: false,
    allowElevation: false,
    allowToChangeInstallationDirectory: true,
    runAfterFinish: false,
    createDesktopShortcut: true,
    createStartMenuShortcut: true,
    shortcutName: 'Vibeface',
    artifactName,
    differentialPackage: false,
  },
  dmg: { artifactName, writeUpdateInfo: false },
  zip: { artifactName },
};

