import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import type { CoreProgram } from '../src/main/control-client.js'

const here = join(fileURLToPath(import.meta.url), '..')

/** A fixture that speaks the real `--control` protocol, never the real Core binary. */
export const CONTROL_FIXTURE: CoreProgram = {
  command: process.execPath,
  args: [join(here, 'fixtures', 'core-control.mjs'), '--control'],
}

export const serviceFixture = (dataDir: string): CoreProgram => ({
  command: process.execPath,
  args: [join(here, 'fixtures', 'core-service.mjs'), '--data-dir', dataDir],
})
