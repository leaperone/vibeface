/** One Noise channel after the handshake; records are at most 65 535 bytes on the wire. */
export interface SecureChannel {
  free(): void
  /** The records for one application message, each sent as its own frame. */
  sealMessage(payload: Uint8Array): Uint8Array[]
  /** The whole message once its last record arrived, otherwise null. */
  openRecord(record: Uint8Array): Uint8Array | null
}

export interface SecureHandshake {
  free(): void
  finish(): SecureChannel
  isFinished(): boolean
  read(message: Uint8Array): Uint8Array
  remoteId(): string | undefined
  setProof(deviceId: string, signature: string): void
  signingBytes(): Uint8Array
  write(payload: Uint8Array): Uint8Array
}

export interface SecureIdentity {
  free(): void
  deviceId(): string
  secretHex(): string
  sign(domain: string, payload: Uint8Array): string
}

export interface SecureMembership {
  free(): void
  createRecoveryCheckpoint(root: SecureIdentity, membersJson: string, headsJson: string, backupEpoch: number): string
  governanceTemplate(issuer: string, actionJson: string): string
  stateJson(): string
  viewJson(): string
}

/**
 * `crates/secure` as each platform loads it: WASM in the browser, a uniffi module on phones.
 * Every call is synchronous and throws an `Error` whose message is the Rust error.
 */
export interface SecureApi {
  Handshake: new (initiator: boolean, expectedDeviceId: string | null | undefined, context: Uint8Array) => SecureHandshake
  Identity: { generate(): SecureIdentity; fromSecretHex(secret: string): SecureIdentity }
  attestationPayload(networkId: string, scopeId: string, bodyJson: string): Uint8Array
  createGenesis(root: SecureIdentity, firstClientJson: string, firstNodeJson: string): string
  governancePayload(eventJson: string): Uint8Array
  inviteSigningPayload(recordJson: string): Uint8Array
  joinCommit(joiner: string, joinerNonce: string): string
  recoveryOpen(envelope: Uint8Array, password: string): Uint8Array
  recoverySeal(payload: Uint8Array, password: string): Uint8Array
  shortCode(networkId: string, inviteId: string, joiner: string, admin: string, adminNonce: string, joinerNonce: string): string
  signingMessage(domain: string, payload: Uint8Array): Uint8Array
  ticketDecode(text: string): string
  ticketEncode(entryId: string, secretHex: string, relayUrl: string | null | undefined, directAddrs: string[]): string
  verify(deviceId: string, domain: string, payload: Uint8Array, signature: string): void
  verifyMembership(stateJson: string, expectedNetwork: string): SecureMembership
}
