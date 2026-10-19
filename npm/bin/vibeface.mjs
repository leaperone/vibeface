#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const targets = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
};
const target = targets[`${process.platform}-${process.arch}`];
if (!target || (process.platform === 'linux' && !process.report.getReport().header.glibcVersionRuntime)) {
  console.error('vibeface supports macOS Apple Silicon and Linux x64/ARM64 with glibc.');
  process.exit(1);
}
const binary = fileURLToPath(new URL(`../vendor/${target}/vibeface`, import.meta.url));
if (!existsSync(binary)) {
  console.error('The Vibeface native package is incomplete. Reinstall with npm install -g vibeface@nightly.');
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });
const handlers = new Map(['SIGINT', 'SIGTERM', 'SIGHUP'].map(signal => {
  const handler = () => child.kill(signal);
  process.on(signal, handler);
  return [signal, handler];
}));
child.on('error', error => {
  console.error(`Cannot start Vibeface: ${error.message}`);
  process.exitCode = 1;
});
child.on('exit', (code, signal) => {
  for (const [name, handler] of handlers) process.off(name, handler);
  if (signal) process.kill(process.pid, signal);
  else process.exitCode = code ?? 1;
});
