# Delivery domain API

Draft implementation in `work/vdp-domain`. Product integration, proof resource acceptance, chain settlement and final review remain separate gates.

## Shared ABI

`abi/Statement.sol` is the only struct declaration. Import `DeliveryStatement` and `DeliveryProfile` from this exact file. Do not reformat this file after compiling a guest without rebuilding its image and profile.

`DeliveryStatement` is 47 static ABI words, 1504 bytes. `priorityDeadline` is slot 42, after `retainUntil`. `keyPurpose`, `payerOrderNonce`, `providerQuoteNonce`, and `bindingAction` are slots 43 through 46. Relations are key_binding=1, source=2, hold=3, open=4. Acceptance is recoverable=0 or strict=1. Binding action is active=1 or revoked=2.

`DeliveryProfile` has these ordered fields.

| Field | Solidity type |
| --- | --- |
| domain | bytes32 |
| guestImageId | bytes32 |
| statementSchemaHash | bytes32 |
| cryptoSuite | uint8 |
| maxWindows | uint16 |
| maxWindowBytes | uint32 |
| maxTotalBytes | uint64 |
| maxGuestCycles | uint64 |
| maxProverMemoryBytes | uint64 |

Its domain is SHA256 of UTF-8 `vibeface-delivery-profile-v1`. `statementSchemaHash` is SHA256 of the exact bytes of `abi/Statement.sol`, including both struct declarations. `profileId = SHA256(abi.encode(profile))`. Rust exports `schema_hash()`, `ProofProfile::abi_bytes()` and `ProofProfile::digest()`.

The dependency order has no back edge.

```text
Statement.sol bytes + fixed guest source
    -> compile guest -> image ID
    -> runtime ProofProfile body -> profile ID
    -> Terms -> encrypted windows -> manifest -> quote -> source signature
    -> proof input -> guest journal
```

The guest contains the schema bytes and algorithms. Its final image and profile ID are runtime data. The verifier binds the actual program image.

## Rust data mapping

`Word` is `[u8;32]` and `Address` is `[u8;20]`. PeerID strings decode to the raw Ed25519 key bytes. EVM addresses decode to 20 bytes. Do not accept another JSON representation as canonical ABI.

`Amount` is an opaque uint256. Use `Amount::from_decimal_str`, `to_decimal_string`, `from_be_bytes`, `to_be_bytes`, `from_u64`, and `checked_add`. Public UI amounts must be decimal strings. The crate's private witness JSON currently uses the underlying U256 serde quantity representation, so it is not the product RPC DTO.

`KeyBinding` owns chain_id, escrow, receiver_peer, receiver_public, key_id, serial, valid_from, valid_until, and action. Purpose is fixed to 1. `SignedKeyBinding` adds the receiver Node's `PeerSignature`. `PeerSignature::from_bytes([u8;64])` and `to_bytes()` adapt the existing secure signing API. No identity seed is a proof witness.

`Terms` owns profile_id, guest_image_id, the unsigned binding, source_peer, provider_peer, payer, provider_wallet, asset, acceptance, total_plaintext_bytes, window_count, whole_order_bonus, escrow_amount, quote_expires_at, hold_deadline, key_deadline, open_deadline, retain_until, priority_deadline and payer_order_nonce. Chain and escrow come from its binding. `priority_deadline` must be strictly greater than `quote_expires_at` and no later than key_deadline for recoverable or open_deadline for strict.

`WindowSpec` owns index, offset, plaintext_bytes, ciphertext_bytes, ciphertext_hash, capsule_hash, plaintext_commitment, delivery_fee, and reservation_fee. The full ordered list must cover the exact total length, have no empty interior windows, and sum to the exact escrow amount including bonus. A zero-byte file is one authenticated empty window.

`UnsignedOrder { terms, windows, quote_nonce }` provides `validate()`, `manifest_abi_bytes()`, `manifest_root()`, `quote_digest()`, `quote_signing_payload()` and `source_signing_payload()`. `Order::signed(unsigned, binding_signature, quote_signature, source_signature)` verifies all three PeerID signatures.

The manifest is exactly `SHA256(abi.encode(tag, preTermsHash, bytes32[] leaves))`. In Rust this dynamic tuple uses `abi_encode_params()`, not `abi_encode()`. Its first words are tag, PreTerms hash, array offset 0x60, array length, and then the leaves. There is no outer 0x20 tuple offset. The native test asserts these exact words and rejects the wrapped hash. `manifestAbi` in public vectors contains these exact parameter bytes. Other transcript tuples are static, so their encoding has no dynamic outer offset.

## Source call sequence

1. Construct and validate the runtime ProofProfile from the fixed deployment. Register a receiver key binding through the chain flow before creating a paid order.
2. Construct Terms with this profile ID and image. Fix the full service budget before encryption.
3. For each window call `encrypt_window(&terms, index, offset, plaintext, &randomness, delivery_fee, reservation_fee)`. It returns `EncryptedWindow { spec, ciphertext, capsule }`.
4. Construct UnsignedOrder from all specs and a provider quote nonce. Ask P to sign `quote_signing_payload()` in `delivery-quote`. Its wallet separately signs the contract's EIP-712 `DeliveryQuote(bytes32 quoteDigest,bytes32 providerPeer)` with the same quote digest.
5. Ask S to sign `source_signing_payload()` in `delivery-manifest`, then construct Order::signed. Signatures use the existing `signing_message` envelope.
6. Construct `Statement::window(&order.unsigned, index, Relation::Source)` and `Witness::Source`. The source witness owns the full window plaintext, source encryption randomness, C, E, and signed Order. It never contains R's secret.

Use C length `plaintext_bytes + 68`, E length 80. All hashes use SHA256. AEAD and HPKE bind only the PreTerms digest and window position. The final manifest, quote and signatures come later.

## Receiver call sequence

1. Verify the source Order, consent policy, budget, final chain facts and manifest before scheduling work.
2. Persist the complete C. Construct the Hold statement and sign `consent_signing_payload()` with the receiver Node's identity in `delivery-accept`.
3. Construct `Witness::Hold` with full C, receiver X25519 `Secret32`, unsigned order, index and consent signature. It contains neither E nor a data key.
4. After the held chain state and published E, use `recover_window()` to retrieve and authenticate plaintext. For strict proof, construct `Witness::Open` with C, E and the receiver secret. Open never accepts a caller-supplied data key or plaintext.

`RecoveredWindow` owns zeroized plaintext and its SHA256. The Node, Web and Mobile must distinguish local recovery from an accepted on-chain open proof.

## Proof boundary

`ProofInput { profile, statement, witness }` is the private stdin format. The complete validation entry is `verify_relation(&input.profile, &input.statement, &input.witness)`. It validates the profile hash, exact schema, image field, protocol caps, phase, canonical ABI, full manifest and phase-specific relation.

`Statement::key_binding(&profile, &signed_binding)` constructs the registration journal. `Statement::from_bytes` rejects noncanonical padding, unknown enums, wrong domain, invalid keys and unexpected key-binding fields. All phase constructors return Result.

`Secret32` and `SecretBytes` zeroize on drop and do not implement Debug or Clone. They are serialized only across the local prover boundary. Secret witnesses must never enter argv, logs, DAG or public fixtures. Independent source, hold and open variants enforce the absence of unrelated secrets by their field shapes.

The chain remains responsible for current time, current registry serial, source-before-hold, held-before-publish, escrow balances, fixed provider and finality. A valid Open relation alone does not establish a paid order or prior source verification.

## Charity object crypto

`encrypt_charity_window(context: Word, receiver_public: Word, index: u16, offset: u64, plaintext: &[u8], randomness: &WindowRandomness)` returns Result<EncryptedWindow>. `recover_charity_window(context: Word, receiver_public: Word, spec: &WindowSpec, secret: &Secret32, ciphertext: &[u8], capsule: &[u8])` returns Result<RecoveredWindow>.

The context must be the nonzero digest of independently signed CharityTerms, including the receiving public key, sender, receiver and provider PeerIDs, nonce, exact size, window count and retention rules. The Node validates these signatures and the full manifest before calling this primitive. Do not construct fake paid Terms, registry state, wallet addresses or profile IDs.

Use `verify_peer()` for the separate Charity key certificate and `validate_receiving_public()` to reject noncanonical or small-order X25519 keys at its boundary. Paid KeyBinding deliberately requires a real chain and escrow and does not stand in for this certificate.

Charity uses the same C and E byte layouts and zeroized random material as paid delivery. Its fee fields are always zero. Limits remain 64 KiB per window, 16 windows and 1 MiB total as protocol bounds, not measured service capacity. AEAD, HPKE info, HPKE AAD and plaintext-commitment labels are respectively `vibeface-delivery-charity-data-v1`, `vibeface-delivery-charity-hpke-info-v1`, `vibeface-delivery-charity-hpke-aad-v1`, and `vibeface-delivery-charity-plain-v1`. The canonical context tuple remains label hash, context digest, uint16 index, uint64 offset and uint32 plaintext length. This API has no chain calls or proof acceptance and must display signature receipt strength only.

## Commands

Native checks from the task worktree.

```sh
cargo test --manifest-path crates/delivery/Cargo.toml --target-dir .artifacts/vdp-domain/target --locked
cargo run --manifest-path crates/delivery/Cargo.toml --target-dir .artifacts/vdp-domain/target --locked --example fixture -- public 35 2 > .artifacts/vdp-domain/public-vectors.json
```

The public vector generator reports profile ABI, all journals, transcript payloads, signatures, leaves, C and E. Its synthetic fixture keys are confined to example and unit-test support. These vectors check serialization and native crypto, not guest proofs.

Without an explicit profile file the example uses the protocol's 16-window and 1-MiB bounds for tests only. Genuine first-release fixtures must supply the frozen two-window profile with max_total_bytes=131072. Pass its public JSON path after image_hex for `public`, or after private_output for `input`. The example validates its schema, image and caps before generating data. These profile fields are runtime data and do not change the guest.

The independent workspace is `crates/delivery/proof`. Set `RISC0_HOME` to a preinstalled compatible RISC Zero toolchain and `CARGO_TARGET_DIR` to an ignored task artifact directory when building. The host uses `risc0-zkvm=3.0.6` client-only with disable-dev-mode. Official SHA2, Curve25519 and crypto-bigint patches live only in this proof workspace and guest workspace.

`vibeface-delivery-prover info` returns this build's image and schema hashes. Generate private input with the fixture example's `input <length> <count> <image_hex> <relation> <index> <new_private_path>` command. It creates a mode-0600 file and refuses overwrite. Run `execute` or `prove <new_public_receipt_path>` with that file on stdin. Set `RISC0_SERVER_PATH` to an absolute trusted local r0vm path. Execute reports cycles and explicitly says proved=false. Prove only exports a genuinely verified Groth16 seal. Its final receipt appears only after a private temporary file has been fsynced and atomically published without replacing another result, followed by directory fsync. An existing final file stops another prove attempt. `verify <expected_public_journal_hex>` reads a serialized public receipt from stdin and returns verified, kind=groth16, the 260-byte selector-plus-seal, imageId and journalHash. Node restarts recover a stable receipt through verify rather than generating another proof.

Host memory enforcement is the Node supervisor's responsibility. A maxProverMemoryBytes field is a signed limit, not evidence that the OS enforced it. No claim of resource acceptance follows from native unit tests or a successful build.
