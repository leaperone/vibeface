# Protected local delivery budgets

This lab supports loopback chain 31337 and pinned six-decimal TestToken only. There is no production asset or public deployment. Node lab and paid gates default to false. Free discovery and charity remain free. The previous unmerged voluntary vouchers and Strict product surface are removed; protected payment requires a new paid order chosen before service.

The canonical `crates/delivery/abi/Statement.sol` remains 47 static words / 1504 bytes; `priorityDeadline` is word 42. Guest, image, relations and official checksum-pinned verifier are unchanged. Source/Hold and correct E establish recoverability, not human reading, physical relay path, exclusive bandwidth or continuous storage.

```sh
node contracts/delivery/install.mjs
node contracts/delivery/compile.mjs --check-trust
node contracts/delivery/exercise-settlement.mjs --self-test
node contracts/delivery/run.mjs
node contracts/delivery/verify-risc0.mjs
node contracts/delivery/verify-delivery.mjs
```

Dependencies are installed under ignored `.artifacts/vdp/contracts/`. `DELIVERY_CONTRACT_ARTIFACTS` selects a separate ignored output directory while reusing installed dependencies. `compile --write-trust` deliberately regenerates tracked runtime templates; CI only uses `--check-trust`. Immutable groups, exact runtime, profile/image and adapter/verifier getters are independently checked. Production bytecode must remain below EIP-170; accounting tests do not use unlimited contract size.

`run.mjs` tests synthetic accounting with a digest verifier, explicitly not ZK: actual two-order reserve/hold/reveal batches, whole-order bonus, fixed payee, idempotence with a keeper winning one item, overspend/domain/proof/E rejection, immutable old-key rights, finite refunds, exact token behavior and earned debt surviving payout failure. Small synthetic plaintexts do not validate the advertised maximum cryptographic capacity. `verify-risc0.mjs` checks the official published verifier fixture. `verify-delivery.mjs` independently verifies the repository's genuine four-relation public seals against the product image and rejects altered seals/journals/images/selectors; host verification must separately bind binary receipts. Neither is a full Node/file/payment acceptance.

## Contract calls

`DeliveryEscrow(DeliveryProfile)` retains its constructor and deterministic CREATE recipe. It constructs the fixed adapter/verifier. `previewBinding`/`registerKey` use relation 1 and increasing serials. `previewTerms` constructs canonical immutable terms once; `preview(input,specs)` constructs one Source statement per window. Domain, chain, escrow, profile, image, order ID, key hash, PreTerms, manifest and quote digest are reconstructed. `publicValues(order,index,relation)` reconstructs Source/Hold/Open (2/3/4). EIP-712 DeliveryQuote still binds quote digest and provider PeerID; peer signatures are separately verified by the guest.

- `openBudget(salt,payee,providerPeer,asset,deposit)` takes one exact six-decimal transfer and permanently fixes roles/asset/deposit. ID binds chain, escrow, actual payer and salt.
- `reserveBatch(budgetId,reservations)` is payer-only and verifies exact Source seals, signed wallet quote, active key at allocation and free balance before service. A batch contains at most two windows in total, including two distinct one-window orders. Exact repeated allocations are idempotent; changed reservations or cross-budget reuse fail.
- `holdBatch(budgetId,items)` verifies each original Hold. Later key rotation/revocation cannot revoke the historical funded binding. There is no old fund/cancel/current-key refund path.
- `revealBatch(budgetId,items)` verifies each fixed 80-byte capsule hash, live Held state and original deadline, stores E and earns fees without a token transfer. Already Earned items skip idempotently so a keeper winning one item does not revert the batch's other valid E.
- `open(order,index,seal)` is optional extra decryption evidence on an already Earned window. Recoverable payment does not depend on Receiver cooperation after Hold.
- `withdrawBudget(id)` transfers only cumulative earned minus withdrawn, to fixed payee. Per-order withdrawn stays zero; budget accounting is authoritative for payouts.
- `closeBudget(id)` is payer-only and blocks new allocations without reclaiming live rights. `refundBudget(id,refs)` takes at most two windows and requires time strictly after the maximum allocated keyDeadline. Earned funds never refund to payer; keepers cannot change payout destinations.

For deposit D: free = D - locked - earned - refunded; actual retained token balance = free + locked + earned - withdrawn. All windows must earn before priorityDeadline for the one whole-order bonus. A fixed17 fixture is 10+2+conditional5; its full-income cutoff is min(keyDeadline, priorityDeadline), never just keyDeadline. A zero bonus does not shorten its key deadline.

## Node publication and cost

The current Node requires a manually configured PublicationPolicy on P. Hold handling queues proofs; explicit `wallet budget flush` fixes a finite batch, then one durable claim path handles both request/background recovery. Full call/payee/budget/cutoff/amount/cap and signed raw+nonce association are durable before any E-bearing RPC. Signed remains private; first possible broadcast rechecks W+I+F+S and marks DisclosedOutstanding durably. E-bearing estimates, calls and simulations are prohibited. A bounded two-window reveal uses fixed offline 800000 gas with no proof verification; tests check finite shapes, and runtime changes require renewed review. Disclosed recovery does not reapply the full first margin. Canonical nonce-consuming reverts permit a new attempt under the same cumulative cap only after all attempts/nullifiers reconcile. Unknown is not failure. Fixed-fee mode does not provide same-nonce automatic replacement.

Each Source and Hold is still independently proved/verified. Every window still stores 80 bytes, a nullifier and earnings. Normal lifecycle writes are key registration, exact approval, budget open, batch reserve, batch Hold, batch reveal and cumulative withdrawal. Optional Open, close/refund, failed attempts and deployment costs are additional. Batching amortizes transaction overhead; it does not eliminate per-window proof, calldata or storage gas. Token freezes preserve earned debt but actual withdrawal depends on recovery. Chain/host delay, reorg, censorship, low gas, locked/off execution or allocation rollback outside the explicit bounds can still break availability; this is not absolute fair exchange.

## Genuine public observation

```sh
node contracts/delivery/observe-settlement.mjs --settlement /absolute/protected-bundle.json --rpc-url http://127.0.0.1:8545
```

Only protected schema2 is accepted. Bundle fields are `schemaVersion=2`, escrow, asset, budgetId, orders, proofs, capsules and transactions. Each order gives `{orderId,windowCount,amount}`; non-key proof entries identify orderId/windowIndex/relation and relative public/receipt filenames. Every window needs Source and Hold; optional Open is separate. Capsules are exported only after disclosure, as 80 public bytes in lowercase bare hex. Transactions identify registerKey, approve, openBudget and arrays reserveBatch/holdBatch/revealBatch/withdrawBudget, plus optional Open.

The observer verifies production trust, canonical profile, genuine seals and reconstructed journals, actual calldata/events, finite batches, budget/order totals and historical payer/payee/escrow balances at one stable two-confirmation experimental anchor. A two-slot bundle must contain a real two-slot transaction in each of reserve/Hold/reveal. It hashes binary receipts; an independent host must verify receipt-to-journal binding. It never signs or broadcasts. Exit 0/1/2 means PASS/FAIL/UNVERIFIED. Listed-operation gas excludes deployment/proving/unlisted recovery; these must be reported separately. Two blocks are an observation policy, not absolute finality.

`exercise-settlement.mjs --self-test` validates schema2 public evidence boundaries without starting Node/chain/proofs. The former Strict JSONL snapshot/mutation protocol is retired and explicitly rejected, as are old Strict/ordinary observer modes. Current money adversarial checks run in `run.mjs`; true protected Node/proof/restart/UI acceptance remains separate.

The tracked native driver is `scripts/run-delivery-live.py --protected-config <owner-only-final-config>`, backed by `verify-node.py --delivery-protected` and `verify-delivery-protected.py`. It consumes final clean-source Linux artifacts, uses the current Node helpers and cgroup checks, and creates five fresh genuine receipts for two separate paid5 orders. It requires actual two-slot reserve/Hold/reveal, the payer offline, provider automatic cumulative withdrawal10, receiver file recovery and S/R/P restart with locked/off gates. `inspect-protected.mjs` reads canonical public chain state for this driver; it neither signs nor submits transactions. `--protected-selfcheck` starts no processes, network sockets or proofs. The config and commands are documented in `packages/docs/e2e.md`; native success does not establish rendered UI or untested fault branches.

`experiment-delivery.mjs` keeps the existing deterministic genuine17 small-file recipe and exact journals, but uses budget entry points. Its raw late/reorg branches intentionally bypass the Node publication gate and remain boundary counterexamples. New runtime/deployment manifests must be separate from immutable old evidence; no old17 proof becomes a fee5 or a second order. See `packages/docs/paid-delivery.md` for actual owner commands and acceptance boundaries.
