'use strict';
// electron-builder calls this for the app, its helper programs and its NSIS installer.
const assert = require('node:assert/strict');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
module.exports = async function sign(configuration, packager) {
  const verifier = await import(pathToFileURL(path.resolve(__dirname, '../../../scripts/windows-signing.mjs')).href);
  const runtime = path.resolve(process.env.VIBEFACE_DESKTOP_OUT, 'win-unpacked/resources/runtime');
  const relative = path.relative(runtime, path.resolve(configuration.path));
  if (relative && !relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative)) {
    // This exact runtime was already signed before its FILES.sha256 was sealed.
    verifier.requireAuthenticode(configuration.path);
    return;
  }
  const manager = await packager.signingManager.value;
  assert(typeof manager.doSign === 'function', 'The pinned electron-builder signtool manager is required');
  await manager.doSign(configuration, packager);
  verifier.requireAuthenticode(configuration.path);
};
