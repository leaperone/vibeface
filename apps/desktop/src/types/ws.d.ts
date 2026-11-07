declare module 'ws' {
  import { EventEmitter } from 'node:events'

  export type RawData = Buffer | ArrayBuffer | Buffer[]
  export type ClientOptions = {
    headers?: Record<string, string>
    followRedirects?: boolean
    handshakeTimeout?: number
    maxPayload?: number
    perMessageDeflate?: boolean
  }
  export type SendOptions = { binary?: boolean; compress?: boolean }

  export default class WebSocket extends EventEmitter {
    static readonly OPEN: number
    readonly readyState: number
    constructor(address: string, options?: ClientOptions)
    on(event: 'open' | 'close', listener: () => void): this
    on(event: 'error', listener: (error: Error) => void): this
    on(event: 'message', listener: (data: RawData, isBinary: boolean) => void): this
    send(data: string | Uint8Array, options: SendOptions, callback: (error?: Error) => void): void
    close(code?: number, reason?: string): void
    terminate(): void
  }
}
