import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = path.dirname(fileURLToPath(import.meta.url));
const destination = path.resolve(root, '../../.artifacts/vdp/contracts');
fs.mkdirSync(destination, { recursive: true });
for (const name of ['package.json', 'package-lock.json']) fs.copyFileSync(path.join(root, name), path.join(destination, name));
const result = spawnSync('npm', ['ci', '--prefix', destination, '--no-audit', '--no-fund'], { stdio: 'inherit' });
process.exit(result.status ?? 1);
