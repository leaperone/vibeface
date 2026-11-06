#!/usr/bin/env node
/** Keeps running until stdin closes — matches parent-held stdin contract. */
import { stdin } from 'node:process'
stdin.resume()
stdin.on('end', () => process.exit(0))
stdin.on('close', () => process.exit(0))
setInterval(() => {}, 1 << 30)
