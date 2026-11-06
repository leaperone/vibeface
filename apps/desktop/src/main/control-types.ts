/** Mirrors `vibeface_control::ControlRequest` (bounded JSON lines). */

export type ControlRequest =
  | { command: 'init'; options: Record<string, unknown> }
  | { command: 'inspect'; data_dir: string }
  | { command: 'access'; data_dir: string; json: boolean }
  | { command: 'set_local_access'; data_dir: string; enabled: boolean }
  | { command: 'join'; data_dir: string; link: string; name: string }
  | { command: 'wallet'; data_dir: string; operation: Record<string, unknown> }

export type ControlResponse = {
  ok: boolean
  result?: unknown
  error?: string
}
