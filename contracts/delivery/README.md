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

Dependencies, bytecode, ABI, cross-language vectors and machine-readable results are written only under ignored `.artifacts/vdp/contracts/`. Installation copies the exact package manifest and lockfile there before `npm ci`. For isolated parallel development only, `DELIVERY_ABI_FILE=/absolute/path/to/Statement.sol` selects the canonical file before it has been integrated into this checkout. No duplicate ABI is maintained here.

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
