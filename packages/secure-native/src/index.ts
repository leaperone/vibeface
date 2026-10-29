import type {
  SecureApi,
  SecureChannel,
  SecureHandshake,
  SecureIdentity,
  SecureMembership,
} from '@vibeface/client/secure-api.ts'
import * as ffi from './generated/index'

/** One client stream to a Node on the LAN, as the client's transport reads it; `recv` gives `null` once the Node ended it. */
export type LanStream = { send(data: Uint8Array): void; recv(): Promise<{ text: boolean; data: Uint8Array } | null>; close(): void }

/** Native identities also sign raw bytes, e.g. a Noise handshake's `signingBytes()`. */
export interface NativeIdentity extends SecureIdentity {
  signMessage(message: Uint8Array): string
}

type NativeSecure = Omit<SecureApi, 'Identity'> & {
  Identity: { generate(): NativeIdentity; fromSecretHex(secret: string): NativeIdentity }
}

function reason(error: unknown): unknown {
  return error instanceof Error && ffi.SecureError.instanceOf(error) ? new Error(error.inner.reason) : error
}

function call<T>(run: () => T): T {
  try {
    return run()
  } catch (error) {
    throw reason(error)
  }
}

function toBuffer(bytes: Uint8Array): ArrayBuffer {
  return new Uint8Array(bytes).buffer
}

function unwrap(identity: SecureIdentity): ffi.Identity {
  if (!(identity instanceof Identity)) throw new Error('identity is not a native secure identity')
  return identity.native
}

class Identity implements NativeIdentity {
  static generate(): Identity {
    return new Identity(call(() => ffi.Identity.generate()))
  }

  static fromSecretHex(secret: string): Identity {
    return new Identity(call(() => ffi.Identity.fromSecretHex(secret)))
  }

  private constructor(readonly native: ffi.Identity) {}

  free(): void {
    this.native.uniffiDestroy()
  }

  deviceId(): string {
    return this.native.deviceId()
  }

  secretHex(): string {
    return this.native.secretHex()
  }

  sign(domain: string, payload: Uint8Array): string {
    return call(() => this.native.sign(domain, toBuffer(payload)))
  }

  signMessage(message: Uint8Array): string {
    return this.native.signMessage(toBuffer(message))
  }
}

class Channel implements SecureChannel {
  constructor(private readonly native: ffi.Channel) {}

  free(): void {
    this.native.uniffiDestroy()
  }

  sealMessage(payload: Uint8Array): Uint8Array[] {
    return call(() => this.native.sealMessage(toBuffer(payload))).map((record) => new Uint8Array(record))
  }

  openRecord(record: Uint8Array): Uint8Array | null {
    const message = call(() => this.native.openRecord(toBuffer(record)))
    return message === undefined ? null : new Uint8Array(message)
  }
}

class Handshake implements SecureHandshake {
  private readonly native: ffi.Handshake

  constructor(initiator: boolean, expectedDeviceId: string | null | undefined, context: Uint8Array) {
    this.native = call(() => new ffi.Handshake(initiator, expectedDeviceId ?? undefined, toBuffer(context)))
  }

  free(): void {
    this.native.uniffiDestroy()
  }

  finish(): SecureChannel {
    return new Channel(call(() => this.native.finish()))
  }

  isFinished(): boolean {
    return this.native.isFinished()
  }

  read(message: Uint8Array): Uint8Array {
    return new Uint8Array(call(() => this.native.read(toBuffer(message))))
  }

  remoteId(): string | undefined {
    return this.native.remoteId()
  }

  setProof(deviceId: string, signature: string): void {
    call(() => this.native.setProof(deviceId, signature))
  }

  signingBytes(): Uint8Array {
    return new Uint8Array(this.native.signingBytes())
  }

  write(payload: Uint8Array): Uint8Array {
    return new Uint8Array(call(() => this.native.write(toBuffer(payload))))
  }
}

class Membership implements SecureMembership {
  constructor(private readonly native: ffi.Membership) {}

  free(): void {
    this.native.uniffiDestroy()
  }

  createRecoveryCheckpoint(root: SecureIdentity, next: string, membersJson: string, headsJson: string): string {
    return call(() => this.native.createRecoveryCheckpoint(unwrap(root), next, membersJson, headsJson))
  }

  createKeyRotation(root: SecureIdentity, next: string): string {
    return call(() => this.native.createKeyRotation(unwrap(root), next))
  }

  governanceTemplate(issuer: string, actionJson: string): string {
    return call(() => this.native.governanceTemplate(issuer, actionJson))
  }

  stateJson(): string {
    return call(() => this.native.stateJson())
  }

  viewJson(): string {
    return call(() => this.native.viewJson())
  }
}

export const secure: NativeSecure = {
  Handshake,
  Identity,
  attestationPayload: (networkId, scopeId, bodyJson) =>
    new Uint8Array(call(() => ffi.attestationPayload(networkId, scopeId, bodyJson))),
  createGenesis: (root, firstClientJson, firstNodeJson) =>
    call(() => ffi.createGenesis(unwrap(root), firstClientJson, firstNodeJson)),
  governancePayload: (eventJson) => new Uint8Array(call(() => ffi.governancePayload(eventJson))),
  inviteSigningPayload: (recordJson) => new Uint8Array(call(() => ffi.inviteSigningPayload(recordJson))),
  joinCommit: (joiner, joinerNonce) => call(() => ffi.joinCommit(joiner, joinerNonce)),
  recoveryOpen: (envelope, password) => new Uint8Array(call(() => ffi.recoveryOpen(toBuffer(envelope), password))),
  recoverySeal: (payload, password) => new Uint8Array(call(() => ffi.recoverySeal(toBuffer(payload), password))),
  shortCode: (networkId, inviteId, joiner, admin, adminNonce, joinerNonce) =>
    call(() => ffi.shortCode(networkId, inviteId, joiner, admin, adminNonce, joinerNonce)),
  signingMessage: (domain, payload) => new Uint8Array(call(() => ffi.signingMessage(domain, toBuffer(payload)))),
  ticketDecode: (text) => call(() => ffi.ticketDecode(text)),
  ticketEncode: (entryId, secretHex, relayUrl, directAddrs) =>
    call(() => ffi.ticketEncode(entryId, secretHex, relayUrl ?? undefined, directAddrs)),
  verify: (deviceId, domain, payload, signature) =>
    call(() => ffi.verify(deviceId, domain, toBuffer(payload), signature)),
  verifyMembership: (stateJson, expectedNetwork) =>
    new Membership(call(() => ffi.verifyMembership(stateJson, expectedNetwork))),
}

/** The app's way to Nodes on its LAN: one QUIC stream to a Node, and the tags Nodes advertise there. */
export const lan = {
  async dial(deviceId: string, addrs: string[], timeoutMs: number): Promise<LanStream> {
    const stream = await ffi.lanConnect(deviceId, addrs, timeoutMs).catch((error) => { throw reason(error) })
    // Freed by the garbage collector: a manual free could race a `recv` still in flight.
    return {
      send: (data) => call(() => stream.send(toBuffer(data))),
      async recv() {
        const frame = await stream.recv().catch((error) => { throw reason(error) })
        return frame ? { text: frame.text, data: new Uint8Array(frame.data) } : null
      },
      close: () => stream.close(),
    }
  },
  names: (tag: string, networkId: string, deviceId: string) => ffi.lanTagMatches(tag, networkId, deviceId),
  /** Binds the QUIC endpoint once, so a dial's time goes to dialing; a failure surfaces on that dial. */
  prepare: () => { ffi.lanPrepare().catch(() => {}) },
}
