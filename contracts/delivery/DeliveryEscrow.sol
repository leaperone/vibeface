// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {DeliveryStatement, DeliveryProfile} from "../../crates/delivery/abi/Statement.sol";
import {RiscZeroAdapter} from "./RiscZeroAdapter.sol";

contract DeliveryEscrow {
    bytes32 public constant DOMAIN = sha256("vibeface-delivery-statement-v1");
    bytes32 private constant QUOTE_TYPEHASH = keccak256("DeliveryQuote(bytes32 quoteDigest,bytes32 providerPeer)");
    uint256 private constant HALF_CURVE_ORDER = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    enum Phase { Funded, Held, KeyPublished, Earned, Refunded }
    struct KeyRecord { bytes32 bindingHash; uint64 serial; uint64 validFrom; uint64 validUntil; bool active; }
    struct WindowSpec {
        uint16 index;
        uint64 offset;
        uint32 plaintextBytes;
        uint32 ciphertextBytes;
        bytes32 ciphertextHash;
        bytes32 capsuleHash;
        bytes32 plaintextCommitment;
        uint256 deliveryFee;
        uint256 reservationFee;
    }
    struct Window { WindowSpec spec; Phase phase; bool decryptionProven; bytes capsule; }
    struct Order {
        DeliveryStatement terms;
        Window[] windows;
        uint256 earned;
        uint256 withdrawn;
        uint256 refunded;
        uint16 earnedWindows;
        uint16 refundedWindows;
        uint8 assetDecimals;
        bool bonusEarned;
        bool bonusRefunded;
    }
    struct Budget {
        address payer;
        address payee;
        bytes32 providerPeer;
        address asset;
        uint256 deposit;
        uint256 locked;
        uint256 earned;
        uint256 withdrawn;
        uint256 refunded;
        uint64 maxClaimUntil;
        uint8 assetDecimals;
        uint8 state;
    }
    struct Reservation {
        DeliveryStatement input;
        WindowSpec[] specs;
        bytes[] seals;
        bytes walletSignature;
    }
    struct WindowRef { bytes32 orderId; uint16 index; }
    struct HoldItem { bytes32 orderId; uint16 index; bytes seal; }
    struct RevealItem { bytes32 orderId; uint16 index; bytes capsule; }

    uint256 public constant MAX_BATCH_WINDOWS = 2;

    DeliveryProfile private profile;
    bytes32 public immutable profileId;
    RiscZeroAdapter public immutable adapter;
    mapping(bytes32 => KeyRecord) public keys;
    mapping(bytes32 => Order) private orders;
    mapping(bytes32 => Budget) private budgets;
    mapping(bytes32 => bytes32) public orderBudget;
    mapping(bytes32 => bytes32) private reservationHash;
    uint256 private entered;

    error Invalid();
    error State();
    error Deadline();
    error Proof();
    error Token();
    error Reentrancy();

    event KeyUpdated(bytes32 indexed peer, uint64 serial, bytes32 bindingHash, bool active);
    event Funded(bytes32 indexed orderId, address indexed payer, uint256 amount);
    event WindowUpdated(bytes32 indexed orderId, uint16 indexed index, Phase phase);
    event DecryptionProven(bytes32 indexed orderId, uint16 indexed index);
    event Withdrawn(bytes32 indexed orderId, address indexed provider, uint256 amount);
    event Refunded(bytes32 indexed orderId, address indexed payer, uint256 amount);
    event BudgetOpened(bytes32 indexed budgetId, address indexed payer, address indexed payee, uint256 amount);
    event BudgetReserved(bytes32 indexed budgetId, bytes32 indexed orderId, uint256 amount);
    event BudgetEarned(bytes32 indexed budgetId, bytes32 indexed orderId, uint16 index, uint256 amount);
    event BudgetWithdrawn(bytes32 indexed budgetId, address indexed payee, uint256 amount);
    event BudgetClosed(bytes32 indexed budgetId);
    event BudgetRefunded(bytes32 indexed budgetId, address indexed payer, uint256 amount);

    modifier nonReentrant() {
        if (entered != 0) revert Reentrancy();
        entered = 1;
        _;
        entered = 0;
    }

    constructor(DeliveryProfile memory p) {
        if (p.domain != sha256("vibeface-delivery-profile-v1") || p.guestImageId == 0
            || p.statementSchemaHash == 0 || p.cryptoSuite != 1 || p.maxWindows == 0 || p.maxWindows > 16
            || p.maxWindowBytes == 0 || p.maxWindowBytes > 65536 || p.maxTotalBytes == 0
            || p.maxTotalBytes > uint64(p.maxWindows) * p.maxWindowBytes
            || p.maxGuestCycles == 0 || p.maxGuestCycles > 64000000
            || p.maxProverMemoryBytes == 0 || p.maxProverMemoryBytes > 17179869184) revert Invalid();
        profile = p;
        profileId = sha256(abi.encode(p));
        adapter = new RiscZeroAdapter(p.guestImageId);
    }

    function getProfile() external view returns (DeliveryProfile memory) { return profile; }
    function getOrder(bytes32 id) external view returns (Order memory) { return _order(id); }
    function getWindow(bytes32 id, uint16 index) external view returns (Window memory) { return _window(_order(id), index); }
    function getBudget(bytes32 id) external view returns (Budget memory) { return _budget(id); }

    function openBudget(bytes32 salt, address payee, bytes32 providerPeer, address asset, uint256 deposit)
        external nonReentrant returns (bytes32 id)
    {
        if (salt == 0 || payee == address(0) || payee == address(this) || payee == msg.sender
            || providerPeer == 0 || asset.code.length == 0 || deposit == 0) revert Invalid();
        id = sha256(abi.encode(sha256("vibeface-delivery-budget-v1"), block.chainid, address(this), msg.sender, salt));
        if (budgets[id].state != 0) revert State();
        uint8 decimals = _decimals(asset);
        if (decimals != 6) revert Token();
        budgets[id] = Budget(msg.sender, payee, providerPeer, asset, deposit, 0, 0, 0, 0, 0, decimals, 1);
        _move(asset, msg.sender, address(this), deposit, true);
        if (_decimals(asset) != decimals) revert Token();
        emit BudgetOpened(id, msg.sender, payee, deposit);
    }

    function reserveBatch(bytes32 budgetId, Reservation[] calldata reservations) external nonReentrant {
        Budget storage budget = _budget(budgetId);
        if (budget.state != 1 || msg.sender != budget.payer || reservations.length == 0) revert State();
        if (_decimals(budget.asset) != budget.assetDecimals) revert Token();
        uint256 count;
        for (uint256 n; n < reservations.length; ++n) {
            Reservation calldata r = reservations[n];
            count += r.specs.length;
            if (count > MAX_BATCH_WINDOWS) revert Invalid();
            DeliveryStatement memory b = this.previewTerms(r.input, r.specs);
            if (b.acceptance != 0 || b.payer != budget.payer || b.providerWallet != budget.payee
                || b.providerPeer != budget.providerPeer || b.asset != budget.asset
                || r.seals.length != r.specs.length) revert Invalid();
            bytes32 commitment = sha256(abi.encode(r));
            if (orderBudget[b.orderId] != 0) {
                if (orderBudget[b.orderId] != budgetId || reservationHash[b.orderId] != commitment) revert State();
                continue;
            }
            if (orders[b.orderId].terms.payer != address(0)) revert State();
            if (block.timestamp > b.quoteExpiresAt || block.timestamp < b.keyValidFrom) revert Deadline();
            if (!_currentKey(b)) revert State();
            if (_recover(quoteWalletDigest(b.quoteDigest, b.providerPeer), r.walletSignature) != budget.payee) revert Invalid();
            if (b.escrowAmount > budget.deposit - budget.locked - budget.earned - budget.refunded) revert Invalid();
            for (uint16 i; i < r.specs.length; ++i) _verify(r.seals[i], _statement(b, r.specs[i], 2));
            Order storage o = orders[b.orderId];
            o.terms = b;
            o.assetDecimals = budget.assetDecimals;
            for (uint16 i; i < r.specs.length; ++i) o.windows.push(Window(r.specs[i], Phase.Funded, false, ""));
            orderBudget[b.orderId] = budgetId;
            reservationHash[b.orderId] = commitment;
            budget.locked += b.escrowAmount;
            if (b.keyDeadline > budget.maxClaimUntil) budget.maxClaimUntil = b.keyDeadline;
            emit Funded(b.orderId, b.payer, b.escrowAmount);
            emit BudgetReserved(budgetId, b.orderId, b.escrowAmount);
        }
    }

    function holdBatch(bytes32 budgetId, HoldItem[] calldata items) external nonReentrant {
        _budget(budgetId);
        if (items.length == 0 || items.length > MAX_BATCH_WINDOWS) revert Invalid();
        for (uint256 n; n < items.length; ++n) {
            HoldItem calldata item = items[n];
            if (orderBudget[item.orderId] != budgetId) revert Invalid();
            Order storage o = _order(item.orderId);
            Window storage w = _window(o, item.index);
            _verify(item.seal, _statement(o.terms, w.spec, 3));
            if (w.phase == Phase.Held || w.phase == Phase.Earned) continue;
            if (w.phase != Phase.Funded) revert State();
            // A later registry update cannot revoke an already funded historical binding.
            if (block.timestamp > o.terms.holdDeadline || block.timestamp > o.terms.keyValidUntil) revert Deadline();
            w.phase = Phase.Held;
            emit WindowUpdated(item.orderId, item.index, Phase.Held);
        }
    }

    function revealBatch(bytes32 budgetId, RevealItem[] calldata items) external nonReentrant {
        Budget storage budget = _budget(budgetId);
        if (items.length == 0 || items.length > MAX_BATCH_WINDOWS) revert Invalid();
        for (uint256 n; n < items.length; ++n) {
            RevealItem calldata item = items[n];
            if (orderBudget[item.orderId] != budgetId) revert Invalid();
            Order storage o = _order(item.orderId);
            Window storage w = _window(o, item.index);
            if (item.capsule.length != 80 || sha256(item.capsule) != w.spec.capsuleHash) revert Invalid();
            // A keeper may have earned one item before the rest of this durable batch.
            if (w.phase == Phase.Earned) continue;
            if (w.phase != Phase.Held) revert State();
            if (block.timestamp > o.terms.keyDeadline) revert Deadline();
            w.capsule = item.capsule;
            uint256 previous = o.earned;
            _earn(o, w);
            uint256 amount = o.earned - previous;
            budget.locked -= amount;
            budget.earned += amount;
            emit WindowUpdated(item.orderId, item.index, Phase.Earned);
            emit BudgetEarned(budgetId, item.orderId, item.index, amount);
        }
        if (budget.state == 2 && budget.locked == 0 && budget.deposit == budget.earned + budget.refunded) budget.state = 3;
    }

    function withdrawBudget(bytes32 id) external nonReentrant {
        Budget storage b = _budget(id);
        uint256 amount = b.earned - b.withdrawn;
        if (amount == 0) revert State();
        b.withdrawn = b.earned;
        _budgetPayout(b, b.payee, amount);
        emit BudgetWithdrawn(id, b.payee, amount);
    }

    function closeBudget(bytes32 id) external nonReentrant {
        Budget storage b = _budget(id);
        if (msg.sender != b.payer || b.state != 1) revert State();
        b.state = b.locked == 0 && b.deposit == b.earned + b.refunded ? 3 : 2;
        emit BudgetClosed(id);
    }

    function refundBudget(bytes32 id, WindowRef[] calldata items) external nonReentrant {
        Budget storage b = _budget(id);
        if (b.state != 2 || items.length > MAX_BATCH_WINDOWS) revert State();
        if (block.timestamp <= b.maxClaimUntil) revert Deadline();
        for (uint256 n; n < items.length; ++n) {
            WindowRef calldata item = items[n];
            if (orderBudget[item.orderId] != id) revert Invalid();
            Order storage o = _order(item.orderId);
            Window storage w = _window(o, item.index);
            uint256 released;
            if (w.phase == Phase.Funded || w.phase == Phase.Held) {
                w.phase = Phase.Refunded;
                ++o.refundedWindows;
                released = w.spec.deliveryFee + w.spec.reservationFee;
                emit WindowUpdated(item.orderId, item.index, Phase.Refunded);
            } else if (w.phase != Phase.Refunded && w.phase != Phase.Earned) revert State();
            released += _takeRefundableBonus(o);
            o.refunded += released;
            b.locked -= released;
            if (released != 0) emit Refunded(item.orderId, b.payer, released);
        }
        uint256 amount = b.deposit - b.locked - b.earned - b.refunded;
        if (amount == 0) revert State();
        b.refunded += amount;
        if (b.locked == 0) b.state = 3;
        _budgetPayout(b, b.payer, amount);
        emit BudgetRefunded(id, b.payer, amount);
    }

    function previewBinding(DeliveryStatement calldata input) public view returns (DeliveryStatement memory b) {
        if (input.relation != 1 || input.receiverPeer == 0 || input.receiverKeySerial == 0
            || input.keyValidFrom >= input.keyValidUntil || (input.bindingAction != 1 && input.bindingAction != 2)) revert Invalid();
        if (input.bindingAction == 1) {
            if (input.receiverPublic == 0 || input.receiverKeyId == 0) revert Invalid();
        } else if (input.receiverPublic != 0 || input.receiverKeyId != 0) revert Invalid();
        b.domain = DOMAIN;
        b.relation = 1;
        b.chainId = block.chainid;
        b.escrow = address(this);
        b.profileId = profileId;
        b.guestImageId = profile.guestImageId;
        b.receiverPeer = input.receiverPeer;
        b.receiverPublic = input.receiverPublic;
        b.receiverKeyId = input.receiverKeyId;
        b.receiverKeySerial = input.receiverKeySerial;
        b.keyValidFrom = input.keyValidFrom;
        b.keyValidUntil = input.keyValidUntil;
        b.keyPurpose = 1;
        b.bindingAction = input.bindingAction;
        b.keyBindingHash = _bindingHash(b);
    }

    function registerKey(DeliveryStatement calldata input, bytes calldata seal) external nonReentrant {
        DeliveryStatement memory b = previewBinding(input);
        if (block.timestamp < b.keyValidFrom || block.timestamp > b.keyValidUntil) revert Deadline();
        if (b.receiverKeySerial <= keys[b.receiverPeer].serial) revert State();
        _verify(seal, b);
        keys[b.receiverPeer] = KeyRecord(b.keyBindingHash, b.receiverKeySerial, b.keyValidFrom, b.keyValidUntil, b.bindingAction == 1);
        emit KeyUpdated(b.receiverPeer, b.receiverKeySerial, b.keyBindingHash, b.bindingAction == 1);
    }

    function preview(DeliveryStatement calldata input, WindowSpec[] calldata specs)
        external view returns (DeliveryStatement[] memory statements)
    {
        DeliveryStatement memory b = this.previewTerms(input, specs);
        statements = new DeliveryStatement[](specs.length);
        for (uint16 i; i < specs.length; ++i) statements[i] = _statement(b, specs[i], 2);
    }

    function publicValues(bytes32 id, uint16 index, uint8 relation) public view returns (DeliveryStatement memory) {
        if (relation < 2 || relation > 4) revert Invalid();
        Order storage o = _order(id);
        return _statement(o.terms, _window(o, index).spec, relation);
    }

    function open(bytes32 id, uint16 index, bytes calldata seal) external nonReentrant {
        Order storage o = _order(id);
        Window storage w = _window(o, index);
        if (w.decryptionProven) revert State();
        if (w.phase != Phase.Earned) revert State();
        _verify(seal, _statement(o.terms, w.spec, 4));
        w.decryptionProven = true;
        emit DecryptionProven(id, index);
        emit WindowUpdated(id, index, w.phase);
    }

    function quoteWalletDigest(bytes32 digest, bytes32 providerPeer) public view returns (bytes32) {
        bytes32 separator = keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256("VibefaceDelivery"), keccak256("1"), block.chainid, address(this)
        ));
        return keccak256(abi.encodePacked(hex"1901", separator, keccak256(abi.encode(QUOTE_TYPEHASH, digest, providerPeer))));
    }

    function previewTerms(DeliveryStatement calldata t, WindowSpec[] calldata specs) external view returns (DeliveryStatement memory b) {
        if (t.relation != 2 || t.receiverPeer == 0 || t.receiverPublic == 0 || t.receiverKeyId == 0 || t.receiverKeySerial == 0
            || t.sourcePeer == 0 || t.providerPeer == 0 || t.payer == address(0) || t.providerWallet == address(0)
            || t.providerWallet == address(this) || t.asset.code.length == 0 || t.acceptance != 0
            || t.providerQuoteNonce == 0 || t.windowCount == 0 || t.windowCount > profile.maxWindows
            || specs.length != t.windowCount || t.totalPlaintextBytes > profile.maxTotalBytes
            || t.keyValidFrom >= t.keyValidUntil || t.quoteExpiresAt < t.keyValidFrom
            || t.quoteExpiresAt > t.holdDeadline || t.holdDeadline >= t.keyDeadline
            || t.keyDeadline > t.openDeadline || t.openDeadline > t.retainUntil || t.retainUntil > t.keyValidUntil
            || t.priorityDeadline <= t.quoteExpiresAt
            || t.priorityDeadline > (t.acceptance == 0 ? t.keyDeadline : t.openDeadline)) revert Invalid();
        b.domain = DOMAIN;
        b.chainId = block.chainid;
        b.escrow = address(this);
        b.profileId = profileId;
        b.guestImageId = profile.guestImageId;
        b.sourcePeer = t.sourcePeer;
        b.receiverPeer = t.receiverPeer;
        b.providerPeer = t.providerPeer;
        b.payer = t.payer;
        b.providerWallet = t.providerWallet;
        b.asset = t.asset;
        b.acceptance = t.acceptance;
        b.receiverPublic = t.receiverPublic;
        b.receiverKeyId = t.receiverKeyId;
        b.receiverKeySerial = t.receiverKeySerial;
        b.keyValidFrom = t.keyValidFrom;
        b.keyValidUntil = t.keyValidUntil;
        b.keyPurpose = 1;
        b.bindingAction = 1;
        b.keyBindingHash = _bindingHash(b);
        b.cryptoSuite = profile.cryptoSuite;
        b.totalPlaintextBytes = t.totalPlaintextBytes;
        b.windowCount = t.windowCount;
        b.wholeOrderBonus = t.wholeOrderBonus;
        b.escrowAmount = t.escrowAmount;
        b.quoteExpiresAt = t.quoteExpiresAt;
        b.holdDeadline = t.holdDeadline;
        b.keyDeadline = t.keyDeadline;
        b.openDeadline = t.openDeadline;
        b.retainUntil = t.retainUntil;
        b.priorityDeadline = t.priorityDeadline;
        b.payerOrderNonce = t.payerOrderNonce;
        b.orderId = sha256(abi.encode(sha256("vibeface-delivery-order-v1"), block.chainid, address(this), t.payer, t.payerOrderNonce));
        b.domain = sha256("vibeface-delivery-preterms-v1");
        b.preTermsHash = sha256(abi.encode(b));
        b.domain = DOMAIN;
        uint64 offset;
        uint256 total = b.wholeOrderBonus;
        bytes32[] memory leaves = new bytes32[](specs.length);
        for (uint16 i; i < specs.length; ++i) {
            WindowSpec calldata s = specs[i];
            if (s.index != i || s.offset != offset || s.plaintextBytes > profile.maxWindowBytes
                || s.ciphertextBytes != s.plaintextBytes + 68
                || (s.plaintextBytes == 0 && !(specs.length == 1 && b.totalPlaintextBytes == 0))
                || s.ciphertextHash == 0 || s.capsuleHash == 0 || s.plaintextCommitment == 0) revert Invalid();
            offset += s.plaintextBytes;
            total += s.deliveryFee + s.reservationFee;
            leaves[i] = sha256(abi.encode(sha256("vibeface-delivery-window-v1"), s));
        }
        if (offset != b.totalPlaintextBytes || total != b.escrowAmount) revert Invalid();
        b.manifestRoot = sha256(abi.encode(sha256("vibeface-delivery-manifest-v1"), b.preTermsHash, leaves));
        b.quoteDigest = sha256(abi.encode(sha256("vibeface-delivery-quote-v1"), b.preTermsHash, b.manifestRoot, t.providerQuoteNonce));
        b.providerQuoteNonce = t.providerQuoteNonce;
    }

    function _bindingHash(DeliveryStatement memory b) private pure returns (bytes32) {
        return sha256(abi.encode(sha256("vibeface-delivery-key-v1"), b.chainId, b.escrow,
            b.receiverPeer, b.receiverPublic, b.receiverKeyId, b.receiverKeySerial, b.keyValidFrom,
            b.keyValidUntil, uint32(1), b.bindingAction));
    }

    function _statement(DeliveryStatement memory b, WindowSpec memory w, uint8 relation) private pure returns (DeliveryStatement memory) {
        b = abi.decode(abi.encode(b), (DeliveryStatement));
        b.relation = relation;
        b.windowIndex = w.index;
        b.windowOffset = w.offset;
        b.windowPlaintextBytes = w.plaintextBytes;
        b.windowCiphertextBytes = w.ciphertextBytes;
        b.ciphertextHash = w.ciphertextHash;
        b.capsuleHash = w.capsuleHash;
        b.plaintextCommitment = w.plaintextCommitment;
        b.windowDeliveryFee = w.deliveryFee;
        b.windowReservationFee = w.reservationFee;
        return b;
    }

    function _verify(bytes calldata seal, DeliveryStatement memory statement) internal view virtual {
        if (!adapter.verify(seal, sha256(abi.encode(statement)))) revert Proof();
    }

    function _currentKey(DeliveryStatement memory b) private view returns (bool) {
        KeyRecord storage k = keys[b.receiverPeer];
        return k.active && k.serial == b.receiverKeySerial && k.bindingHash == b.keyBindingHash
            && block.timestamp >= k.validFrom && block.timestamp <= k.validUntil;
    }

    function _earn(Order storage o, Window storage w) private {
        w.phase = Phase.Earned;
        o.earned += w.spec.deliveryFee + w.spec.reservationFee;
        ++o.earnedWindows;
        if (o.earnedWindows == o.windows.length && !o.bonusRefunded && block.timestamp <= o.terms.priorityDeadline) {
            o.bonusEarned = true;
            o.earned += o.terms.wholeOrderBonus;
        }
    }

    function _takeRefundableBonus(Order storage o) private returns (uint256) {
        if (!o.bonusEarned && !o.bonusRefunded && (o.refundedWindows != 0 || block.timestamp > o.terms.priorityDeadline)) {
            o.bonusRefunded = true;
            return o.terms.wholeOrderBonus;
        }
        return 0;
    }

    function _budget(bytes32 id) private view returns (Budget storage b) {
        b = budgets[id];
        if (b.state == 0) revert State();
    }

    function _budgetPayout(Budget storage b, address destination, uint256 amount) private {
        if (_decimals(b.asset) != b.assetDecimals) revert Token();
        _move(b.asset, address(this), destination, amount, false);
        if (_decimals(b.asset) != b.assetDecimals) revert Token();
    }

    function _balance(address asset, address owner) private view returns (uint256) {
        (bool ok, bytes memory data) = asset.staticcall(abi.encodeWithSignature("balanceOf(address)", owner));
        if (!ok || data.length != 32) revert Token();
        return abi.decode(data, (uint256));
    }

    function _decimals(address asset) private view returns (uint8) {
        (bool ok, bytes memory data) = asset.staticcall(abi.encodeWithSignature("decimals()"));
        if (!ok || data.length != 32) revert Token();
        return abi.decode(data, (uint8));
    }

    function _move(address asset, address from, address to, uint256 amount, bool incoming) private {
        if (amount == 0) return;
        uint256 beforeFrom = _balance(asset, from);
        uint256 beforeTo = _balance(asset, to);
        bytes memory callData = incoming ? abi.encodeWithSignature("transferFrom(address,address,uint256)", from, to, amount)
            : abi.encodeWithSignature("transfer(address,uint256)", to, amount);
        (bool ok, bytes memory result) = asset.call(callData);
        if (!ok || (result.length != 0 && (result.length != 32 || !abi.decode(result, (bool))))) revert Token();
        uint256 afterFrom = _balance(asset, from);
        uint256 afterTo = _balance(asset, to);
        if (beforeFrom < amount || afterFrom != beforeFrom - amount || afterTo != beforeTo + amount) revert Token();
    }

    function _recover(bytes32 digest, bytes calldata signature) private pure returns (address signer) {
        if (signature.length != 65) revert Invalid();
        bytes32 r;
        bytes32 s;
        uint8 v;
        assembly { r := calldataload(signature.offset) s := calldataload(add(signature.offset, 32)) v := byte(0, calldataload(add(signature.offset, 64))) }
        if ((v != 27 && v != 28) || uint256(s) > HALF_CURVE_ORDER) revert Invalid();
        signer = ecrecover(digest, v, r, s);
        if (signer == address(0)) revert Invalid();
    }

    function _order(bytes32 id) private view returns (Order storage o) {
        o = orders[id];
        if (o.terms.payer == address(0)) revert State();
    }

    function _window(Order storage o, uint16 index) private view returns (Window storage w) {
        if (index >= o.windows.length) revert Invalid();
        w = o.windows[index];
    }
}
