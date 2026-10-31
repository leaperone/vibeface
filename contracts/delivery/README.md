# Local delivery settlement

This is an experimental settlement implementation. No production asset, public escrow address or deployment is configured. All four delivery relations require genuine proofs in `DeliveryEscrow`. `AccountingTestEscrow` accepts deterministic statement digests solely to test accounting and must never be deployed for funds.

The canonical ABI is `crates/delivery/abi/Statement.sol`. It contains 47 static words, exactly 1504 bytes; `priorityDeadline` is word 42. Solidity imports that file directly. The compiler hashes and checks every vendored official verifier file against `verifier-sources.json`. RISC Zero sources are pinned to `risc0-ethereum/v3.0.1`; their original copyright and GPL notices remain intact. OpenZeppelin SafeCast is pinned to v5.0.2. The official constants and verification key come from that release, not a business-specific setup ceremony. See the source URLs and checksums in the manifest.

Run from the repository root.

```sh
node contracts/delivery/install.mjs
node contracts/delivery/run.mjs
node contracts/delivery/verify-risc0.mjs
node contracts/delivery/compare-vectors.mjs /absolute/path/to/public-vectors.json
```

Dependencies, bytecode, ABI, cross-language vectors and machine-readable results are written only under ignored `.artifacts/vdp/contracts/`. Installation copies the exact package manifest and lockfile there before `npm ci --ignore-scripts`; the verifier uses Ganache's JavaScript fallback, so optional native install scripts are not needed. The install also bounds registry retries and the command itself so a hosted runner cannot hold the proof deadline. For isolated parallel development only, `DELIVERY_ABI_FILE=/absolute/path/to/Statement.sol` selects the canonical file before it has been integrated into this checkout. No duplicate ABI is maintained here.

`trust.json` is the product's versioned runtime template allowlist for local Node validation. `node contracts/delivery/compile.mjs --write-trust` regenerates it from the exact compiler/settings and canonical ABI. A validator must check that all references within each immutable group contain identical bytes, zero precisely the listed ranges in `eth_getCode`, compare every remaining byte, then independently check profile/image and adapter/verifier getters. Range/group metadata must agree without overlaps or out-of-bounds references. User-provided code hashes are not trusted. `TestToken` is included only for the local asset profile; the accounting escrow is deliberately absent.

`node contracts/delivery/compile.mjs --check-trust` compares fresh compiler output with the tracked trust artifact and fails on any difference. It writes only ignored build artifacts. CI uses this command and never regenerates tracked trust metadata automatically.

`run.mjs` checks money conservation after each scenario using a deterministic **test** verifier. Its 64 KiB cases validate accounting bounds, not 64 KiB cryptographic capacity. `verify-risc0.mjs` verifies the official published Groth16 receipt on a genuine local EVM, including altered seal, image, journal and selector rejection. That fixture is not a newly generated delivery proof. New delivery guest receipts must still be tested separately with `DeliveryEscrow` and the selected guest image.

`node contracts/delivery/verify-delivery.mjs [fixture-directory]` requires a current guest fixture manifest, all four binary receipts and the matching host `prove` JSON outputs. The default directory is `proofs/delivery/fixtures`. Missing artifacts fail; there is no mock fallback. It checks the exact schema, canonical journals and coherent order/key/profile, then verifies each seal with the production adapter and mutated seal/journal/image/selector negatives. Its result describes adapter proof acceptance only. The host fixture verifier must separately bind each binary receipt and metadata to the freshly built guest ELF; complete escrow funding acceptance remains a separate test.

## API

Deploy `DeliveryEscrow(DeliveryProfile)`. The profile fixes image ID, exact canonical ABI file SHA-256, crypto suite and bounded proof resource limits. Its domain is SHA-256 of `vibeface-delivery-profile-v1`; its ID is SHA-256 of its canonical ABI bytes. The guest independently checks the schema hash. The escrow constructs its own `RiscZeroAdapter`, which constructs the checksum-pinned official verifier with fixed control root, BN254 control ID and selector. There is no caller-supplied verifier, journal, upgrade hook or owner bypass.

Call `previewBinding(bindingTemplate)` and `registerKey(bindingTemplate, seal)` for relation 1. The template supplies only receiving identity/key, serial, validity and action; all other public fields are rebuilt. Active action is 1; revocation action is 2 and requires zero receiving key/public fields. Serials strictly increase.

Call `preview(sourceTemplate, WindowSpec[])` before producing source receipts. The input must have relation 2. Derived domain, chain, escrow, profile/image, order ID, key hash, PreTerms hash, manifest, quote digest and window fields are overwritten. `providerQuoteNonce` is an input to the final quote, not PreTerms. `WindowSpec` fields in order are `index`, `offset`, `plaintextBytes`, `ciphertextBytes`, `ciphertextHash`, `capsuleHash`, `plaintextCommitment`, `deliveryFee`, `reservationFee`. The return contains one complete canonical source statement per window.

The provider wallet signs EIP-712 `DeliveryQuote(bytes32 quoteDigest,bytes32 providerPeer)` with domain name `VibefaceDelivery`, version `1`, current chain ID and escrow address. `quoteWalletDigest` returns the exact signing digest. The payer calls `fund(sourceTemplate, specs, sourceSeals[], walletSignature)` after approving the exact asset. Peer signatures are separately verified by the guest; a wallet signature does not substitute for them.

`publicValues(orderId,index,relation)` builds the statement for source/hold/open relations 2/3/4 from immutable stored order data. Any keeper may submit `hold(orderId,index,seal)`, `publish(orderId,index,capsule)`, `open(orderId,index,seal)`, `refund(orderId,index)`, `refundBonus(orderId)` or `withdraw(orderId)`. The capsule is exactly 80 bytes and must match its committed SHA-256. Payouts always go to the fixed provider/payer; keepers choose no destination. Only the payer can `cancel(orderId)` before any live held, key-published or earned window exists.

`getOrder`, `getWindow`, `keys`, `getProfile`, `profileId` and `adapter` expose state and trust roots. Window phase values are Funded 0, Held 1, KeyPublished 2, Earned 3, Refunded 4. Decryption evidence is a separate flag. Earned, withdrawn and refunded amounts are independent. There is no capsule deletion entry point.

Ordinary completion earns fees only when the full capsule is atomically stored after a valid hold. Strict completion requires open before its deadline. Priority bonus is independent of hard acceptance deadlines and refunds separately once impossible. Rotation invalidates unheld old-key windows while preserving held rights. Exact incoming and outgoing balance differences reject fee/rebasing transfer behavior; precision changes, false returns, freezes and reentrant state mutation fail atomically. Non-returning tokens work if their balances and precision are exact. This behavior is not a substitute for auditing and pinning a production asset, proxy and verifier deployment.

## Observe a Node settlement

`observe-settlement.mjs` observes the original local chain without signing or broadcasting. It uses the fixed product profile in `proofs/delivery/profile.json` and checks production runtime templates, all seven genuine public seals, contract-constructed journals, successful transaction calldata and events, both completed windows, retained capsules, actual token balance changes and execution gas fees. The exporter must separately run the host verifier to bind each binary receipt to its public JSON. The observer records each binary receipt's hash but verifies the public EVM seal. Indexed files must be ordinary files beside the public bundle, with relative basenames. The bundle contains no witnesses, private keys or raw transactions.

```sh
node contracts/delivery/observe-settlement.mjs --settlement /absolute/path/to/bundle.json --rpc-url http://127.0.0.1:8545
```

The public bundle has this schema. Addresses have a `0x` prefix. The order ID and transaction hashes are 32 bytes with a `0x` prefix. Public metadata follows the host's lowercase hex format without a prefix. Each capsule is exactly 80 public bytes without a prefix.

```json
{
  "schemaVersion": 1,
  "escrow": "0x<40 hex characters>",
  "asset": "0x<40 hex characters>",
  "orderId": "0x<64 hex characters>",
  "proofs": [
    { "relation": 1, "public": "key.json", "receipt": "key.receipt" },
    { "relation": 2, "windowIndex": 0, "public": "source-0.json", "receipt": "source-0.receipt" },
    { "relation": 3, "windowIndex": 0, "public": "hold-0.json", "receipt": "hold-0.receipt" },
    { "relation": 4, "windowIndex": 0, "public": "open-0.json", "receipt": "open-0.receipt" },
    { "relation": 2, "windowIndex": 1, "public": "source-1.json", "receipt": "source-1.receipt" },
    { "relation": 3, "windowIndex": 1, "public": "hold-1.json", "receipt": "hold-1.receipt" },
    { "relation": 4, "windowIndex": 1, "public": "open-1.json", "receipt": "open-1.receipt" }
  ],
  "capsules": [
    { "windowIndex": 0, "capsule": "<160 lowercase hex characters>" },
    { "windowIndex": 1, "capsule": "<160 lowercase hex characters>" }
  ],
  "transactions": {
    "registerKey": "0x<actual transaction hash>",
    "approve": "0x<actual transaction hash>",
    "fund": "0x<actual transaction hash>",
    "hold": ["0x<window 0 hash>", "0x<window 1 hash>"],
    "publish": ["0x<window 0 hash>", "0x<window 1 hash>"],
    "open": ["0x<window 0 hash>", "0x<window 1 hash>"],
    "withdraw": ["0x<first actual withdrawal hash>", "0x<second actual withdrawal hash>"]
  }
}
```

`approve` is optional. `withdraw` contains one or two actual transactions. Array positions for hold, publish and open identify the window. Gas totals cover listed operations and exclude deployments, faucets, proving costs and any unlisted pre-existing approval.

The local fixture uses a fixed two-confirmation experimental policy. The observer reads at `latest - 1`, checks transaction block hashes and verifies the anchor hash again afterward. This does not establish absolute finality. Exit 0 means `PASS`, exit 1 means `FAIL`, and exit 2 means `UNVERIFIED`. Missing or shallow receipts, unavailable historical balances, a changed anchor, unexplained token movements or money transactions sharing a block prevent a receipt claim. The current local token has no Transfer events, so this tool requires isolated money transactions to attribute historical balance changes. The machine-readable result is `.artifacts/vdp/contracts/settlement-observation.json`. Repeating the observation sends no transactions.

The default observer case remains strict: two windows, seven proofs, acceptance 1, fees 10 + 2 per window, bonus 5, and total earned and withdrawn 29. The ordinary UI supplement is a separate case.

```sh
node contracts/delivery/observe-settlement.mjs --settlement /absolute/path/to/ordinary-settlement.json --rpc-url http://127.0.0.1:8545 --case ordinary
```

Ordinary requires one window, four public proofs, recoverable acceptance 0, total 17, final active Register4 key binding and its actual registration transaction. It checks the expected envelope for the 4096-byte `ordinary-paid-ui.bin`; the UI runner separately checks the recovered file bytes and SHA. `proofs` contains relation 1 and window 0 relations 2, 3 and 4. Each action array has one transaction. The report is `.artifacts/vdp/contracts/ordinary-settlement-observation.json`; the strict report is preserved. Stdout is the complete JSON for this invocation, so the runner can save it beside the corresponding bundle. Shared registration references count once by transaction hash when aggregating task gas.

## Exercise cached genuine branches

`exercise-settlement.mjs` is a finite JSONL controller on the original local 31337 RPC. It never reads Node directories, witnesses, wallet keys or unbroadcast raw transactions, and never copies Node data. Before branches it requires the existing strict observer to pass the original genuine bundle. It reconstructs only already-broadcast signed transactions from public signatures and checks their hashes. Snapshot and branch commands are not a general RPC interface.

```sh
node contracts/delivery/exercise-settlement.mjs --settlement /absolute/path/to/future-strict-bundle.json --rpc-url http://127.0.0.1:8545 --deployment /absolute/path/to/public-deployment.json
```

Start the controller while the original deployment is alive, before the initial prepare. The future bundle path need not exist until `verify-core`. Each input is one JSON object with a unique `id` label and an `op`. Each output is one public JSON event. The acceptance runner alone stops and starts the task S/R/P processes, checks that inherited provers have ended, and copies their complete stopped data into private task checkpoints. It never applies this mechanism to installed Nodes or another task. Branch data and signed raws remain backed up; no table or raw row is deleted to clean a branch.

Every mutating command includes `quiesced` with `nodesStopped:true`, `walletsLocked:true`, `proversIdle:true`, and a bounded opaque `nodeDataCheckpoint` label. Labels are not paths or commands.

| Operation | Additional fields and effect |
| --- | --- |
| `capture-pre-prepare` | `wallets:{S,R,P}` contains public Ethereum addresses, and `receiverPeer` is a 0x-prefixed 32-byte PeerID. Captures C0 after genuine registration and faucets, before prepare. |
| `capture-source-ready` | Same `wallets` and `receiverPeer`, plus `orderId`. Captures C0Prime at the real approve-estimate barrier, after both source proofs and before any approve or fund transaction. |
| `capture-post-fund` | Same `wallets`, plus `orderId` and `fundTxHash`. Captures C1 after two-confirmation fund/offer, before accept and any hold. Both windows must actually be Funded. |
| `resume-main` | Only `id/op`. Resumes mining after a capture or restoration. The runner separately resumes Nodes. |
| `verify-core` | `fileSha256` is the original file SHA without 0x. Uses the existing strict observer, caches public original signatures and seven seals, and associates the complete `MainSuccessData` checkpoint. |
| `exercise-case` | `case` is one of the finite names below. Reverts/recreates C1, requests owner data handling, runs the case, then resets C1 again. |
| `restore-core` | Replays original signed raws with original nonces, restores matching owner-held data and reruns the unmodified strict success oracle. No new signature or replacement fee is generated. |
| `lifecycle-held` | After core restoration, reverts C1, replays original hold0 and requests the sole real Rotate from the owner. Checks mixed held/unheld rights and actual old-key file read. Requires another `restore-core` afterward. |
| `record-key` | `action` is `rotate`, `revoke` or `register`, with the actual `txHash`. Observes a real owner operation already performed on the restored chain. After mixed-held Rotate, continue with Revoke and Register only. |
| `abort` | Stops mining and preserves backups. Does not pretend the chain or Node data has been restored. |

Finite C1 cases are `offline-refund`, `partial-refund`, `held-timeout`, `strict-late-open`, `late-priority`, `hold-reorg`, `capsule-reorg`, `keeper`, `token-false`, `token-frozen`, `token-fee`, `token-sender-tax`, `token-precision`, `token-nonreturn`, `token-reentrancy`, and `node-refund-hook`. They use the same seven genuine proofs and original terms. The late-priority outcome is earned/withdrawn 24 and bonus refund 5, never strict core success 29.

The seven incoming cases are `incoming-false`, `incoming-frozen`, `incoming-fee`, `incoming-sender-tax`, `incoming-precision`, `incoming-reentrancy`, and `incoming-nonreturn`. They restore C0Prime, or C0 if no source-ready barrier was captured, and replay only the original public approve and fund transactions. They then rebuild C1 from those same signed bytes and require the runner to restore its matching C1 data. Snapshot capture alone is not a passing incoming test. Each failed transaction must reproduce its expected contract error with the original gas limit at its receipt block; an unrelated failure or exhausted gas cannot pass.

`node contracts/delivery/exercise-settlement.mjs --self-test` checks finite command and acknowledgement boundaries, public transaction reconstruction and confirmation metadata without starting a chain or generating a proof. Actual branches still require the original genuine strict bundle.

At each `checkpoint`, the controller pauses chain actions and emits `checkpointId`, current public `state`, the two-confirmation anchor, `nodeMode`, and the requested `nodeDataCheckpoint`. The runner handles the matching whole task data and replies as follows.

```json
{
  "op": "checkpoint-ack",
  "checkpointId": "case-id-result",
  "quiesced": { "nodesStopped": true, "walletsLocked": true, "proversIdle": true, "nodeDataCheckpoint": "BranchData" },
  "nodeObservation": "observed",
  "rawBroadcastDelta": 0,
  "views": {
    "R": { "phase": "partial", "evidence": "none", "payment": { "earned": "12", "withdrawn": "12", "refunded": "17" },
      "windows": [{ "state": "earned", "decryption_proven": true }, { "state": "refunded", "decryption_proven": false }] }
  }
}
```

For data restore alone, use `nodeObservation:"not_observed"` and omit `views`. Observed roles are recorded explicitly. The controller compares supplied public money/window/proof projections with the real chain, and permits no raw broadcast in a locked observation. Optional `readSha256` must match the original file. A read after reorg does not prove that already-seen E or plaintext was erased.

`owner-native-refund` checkpoints resume the miner while the acceptance runner restores C1 data and unlocks only the required task Source wallet to execute its actual refund. Afterward the runner locks/stops everything, privately retains the branch data/raws, and includes actual `nodeTransactions` in the acknowledgement. The helper verifies successful Source-signed refund calls and exact chain balances; it does not send the refund. Keeper cases remain explicitly distinct from Node autonomy.

`owner-key-rotate` is the only point that requests the first real Rotate. The owner calls its existing asynchronous key helper once with a new UUID, then stops Nodes and preserves a distinct whole `AfterRotateData` checkpoint. Its acknowledgement includes the sole actual Rotate hash in `nodeTransactions`; order views may be omitted at this stage. The subsequent mixed-held-result checkpoint requires an observed receiver view and the actual old-key `readSha256`. Do not generate another Rotate after canonical restoration: `restore-core` replays the same original Rotate raw after the original success transactions, preserving new key2 while restoring the original order. It refreshes the Rotate observation to its current canonical receipt. Revoke3 and Register4 then advance once on that canonical chain through the owner's CLI, followed by `record-key`. Restoration is forbidden after those canonical lifecycle operations.

New certificates use real host time. If it cannot fit the immutable old hold/key deadlines, the mixed-held case is UNVERIFIED and needs an appropriately planned order; the tool never backdates a certificate or changes a journal. Node-data snapshots are paired deliberately with chain checkpoints by the runner. They are not evidence that chain reorgs automatically roll back local state.

Public outputs are `.artifacts/vdp/contracts/branch-observations/<sessionId>/index.json`, `branch-<case>.json`, `key-rotate.json`, `key-revoke.json`, `key-register.json`, and `core-restored.json`. Branch status `EVM_PASS` describes the named genuine-backed chain case and separately records observed Node roles/autonomy. It is not the strict core oracle. The acceptance exporter owns the sole `key-lifecycle/summary.json` and actual receipt/public files; these observations do not replace host verification or its unique-proof accounting.

Already-broadcast signed raw backups and private checkpoint acknowledgements are kept with 0700/0600 permissions under `.artifacts/vdp/contracts/private-branches/<sessionId>/`. Never upload this directory or full Node checkpoints. Only explicit public output files are collectible. Snapshot IDs expire when reverted or when the original Ganache exits. A case error stops mining and requires explicit restoration or abort. The session cannot claim completion without the final genuine strict observer passing on current canonical receipts. This tool has no proving, Node spawning or device control entry point.
## Small-file settlement experiment

The default-off `delivery_small` Actions dispatch builds the canonical guest and runs `experiment-delivery.mjs` in separate `prepare`, `relation-1` through `relation-4`, and `accept` work items. Preparation replays two fresh deterministic `deploy-local.mjs` deployments. Each serial relation item generates its genuine 4 KiB proof or independently verifies a saved qualified pair. Acceptance requires all four qualified proofs before registering the key, funding and holding the same order. It then checks actual TestToken balances, receipts, events and offline file recovery for normal settlement, pending capsule disclosure followed by a late reverted publish and refund, and a controlled publish rollback and refund. Local `python3 scripts/check-delivery.py --sandbox --prove-small prepare` runs only the non-proof preparation controls.

The adapter-only `verify-delivery.mjs` keeps its existing meaning. The experiment calls it separately, then exercises the ordinary production escrow and verifier, without an accounting stub. Public reports distinguish Token atoms from native gas and independent snapshot branches. The recovery program receives only Hold and a capsule observed in a real transaction; it does not query the chain. See [the CI instructions](../../docs/ci.md#手动生成真实付费交付证据) for the default-off manual switch, resource measurements and public artifact allowlist.
