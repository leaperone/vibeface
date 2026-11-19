#!/usr/bin/env node
import assert from 'node:assert/strict';
import { loadRuntimeCorpus, runRuntimeCorpus, validateRuntimeCorpus } from './runtime-contract.mjs';
const runtime = loadRuntimeCorpus();
validateRuntimeCorpus(runtime);
const outcomes = runRuntimeCorpus(runtime);
if (process.argv.includes('--json')) process.stdout.write(JSON.stringify(outcomes) + '\n');
else console.log(`runtime contract corpus passed: ${outcomes.length} literal Node cases; target, wire, BUILD-INFO, FILES.sha256, roles and seal invariants.`);
