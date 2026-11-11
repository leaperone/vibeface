import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import type { E2EConfig } from 'e2e';
import nativeConfig from './e2e.native.config';

const artifacts = resolve(import.meta.dirname, '../../.artifacts/native/workspace');
const results = resolve(import.meta.dirname, '.artifacts/workspace-native');
const invitation = process.env.VIBEFACE_E2E_NATIVE_INVITATION_FILE ?? resolve(artifacts, 'invitation.txt');
const inside = relative(artifacts, resolve(invitation));
assert(inside && !inside.startsWith('..') && !inside.startsWith('/'), 'The workspace invitation remains in its private task directory');

export default {
  ...nativeConfig,
  tests: ['workspace.native.ts'],
  targets: [{ ...nativeConfig.targets[0], name: 'workspace-ios' }],
  output: results,
  cache: { dir: resolve(results, 'cache') },
  timeout: 600000,
  secrets: { nativeInvitation: async () => (await readFile(invitation, 'utf8')).trim(), nativeWebhookSecret: randomBytes(24).toString('base64url') },
} satisfies E2EConfig;
