import { existsSync } from 'node:fs'
import { join } from 'node:path'
import { app } from 'electron'
import type { CoreProgram } from './control-client.js'
import type { RuntimeState } from '../shared/contract.js'

/**
 * A packaged app only ever runs the runtime sealed into its own resources. Development
 * overrides and test fixtures exist solely for an unpackaged build, so a signed install can
 * never be pointed at another executable through the environment or a deeplink.
 */
export function runtimeRoot(): string {
  if (app.isPackaged) return join(process.resourcesPath, 'runtime')
  const override = process.env['VIBEFACE_DESKTOP_RUNTIME']?.trim()
  if (override) return override
  return join(app.getAppPath(), 'fixtures', 'runtime')
}

export function coreBinary(): string {
  return join(runtimeRoot(), process.platform === 'win32' ? 'vibeface-core.exe' : 'vibeface-core')
}

export function cliBinary(): string {
  return join(runtimeRoot(), process.platform === 'win32' ? 'vibeface.exe' : 'vibeface')
}

export function runtimeState(): RuntimeState {
  const corePath = coreBinary()
  if (existsSync(corePath)) return { kind: 'ready', corePath }
  return {
    kind: 'missing',
    corePath,
    detail: app.isPackaged
      ? '这个安装包缺少捆绑的 Core 程序，请重新安装 Vibeface 桌面版。'
      : '未打包运行时请把 VIBEFACE_DESKTOP_RUNTIME 指向解包后的 CLI 目录。',
  }
}

/** One bounded control request: the fixed Core program plus `--control`, nothing caller supplied. */
export function controlProgram(): CoreProgram {
  if (!app.isPackaged) {
    const fake = process.env['VIBEFACE_DESKTOP_FAKE_CONTROL']?.trim()
    if (fake) return { command: fake, args: ['--control'] }
  }
  return { command: coreBinary(), args: ['--control'] }
}

/** The long-lived owned Core: the fixed program and its own data directory, nothing else. */
export function serviceProgram(dataDir: string): CoreProgram {
  if (!app.isPackaged) {
    const fake = process.env['VIBEFACE_DESKTOP_FAKE_CORE']?.trim()
    if (fake) return { command: fake, args: ['--data-dir', dataDir] }
  }
  return { command: coreBinary(), args: ['--data-dir', dataDir] }
}
