import { useSyncExternalStore } from 'react'
import { storageOf } from '../workspace/storage.ts'
import type { KeyValue } from './platform.ts'

const listeners = new Set<() => void>()
const listen = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener) } }

/** 实验功能 in settings: features still being tried, such as paid delivery on a test chain. Off unless turned on here. */
export const experimentsCopy = { label: '实验功能', hint: '付费传输等还在测试的功能' } as const

export const experimentsOn = (storage: KeyValue): boolean => storageOf(storage).readStorage<unknown>('experiments', false) === true

export function setExperiments(storage: KeyValue, on: boolean) {
  storageOf(storage).writeStorage('experiments', on)
  for (const listener of listeners) listener()
}

/** The switch as every screen of this app or page sees it, updated wherever it is flipped. */
export function useExperiments(storage: KeyValue): [boolean, (on: boolean) => void] {
  const on = useSyncExternalStore(listen, () => experimentsOn(storage))
  return [on, (value) => setExperiments(storage, value)]
}
