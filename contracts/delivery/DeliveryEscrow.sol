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

    DeliveryProfile private profile;
    bytes32 public immutable profileId;
    RiscZeroAdapter public immutable adapter;
    mapping(bytes32 => KeyRecord) public keys;
    mapping(bytes32 => Order) private orders;
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
        DeliveryStatement memory b = _terms(input, specs);
        statements = new DeliveryStatement[](specs.length);
        for (uint16 i; i < specs.length; ++i) statements[i] = _statement(b, specs[i], 2);
    }

    function publicValues(bytes32 id, uint16 index, uint8 relation) public view returns (DeliveryStatement memory) {
        if (relation < 2 || relation > 4) revert Invalid();
        Order storage o = _order(id);
        return _statement(o.terms, _window(o, index).spec, relation);
    }

    function fund(DeliveryStatement calldata input, WindowSpec[] calldata specs, bytes[] calldata seals, bytes calldata walletSignature)
        external nonReentrant returns (bytes32 id)
    {
        DeliveryStatement memory b = _terms(input, specs);
        id = b.orderId;
        if (b.payer != msg.sender || orders[id].terms.payer != address(0) || seals.length != specs.length) revert State();
        if (block.timestamp > b.quoteExpiresAt || block.timestamp < b.keyValidFrom) revert Deadline();
        if (!_currentKey(b)) revert State();
        if (_recover(quoteWalletDigest(b.quoteDigest, b.providerPeer), walletSignature) != b.providerWallet) revert Invalid();
        for (uint16 i; i < specs.length; ++i) _verify(seals[i], _statement(b, specs[i], 2));
        Order storage o = orders[id];
        o.terms = b;
        o.assetDecimals = _decimals(b.asset);
        for (uint16 i; i < specs.length; ++i) o.windows.push(Window(specs[i], Phase.Funded, false, ""));
        _move(b.asset, b.payer, address(this), b.escrowAmount, true);
        if (_decimals(b.asset) != o.assetDecimals) revert Token();
        emit Funded(id, b.payer, b.escrowAmount);
    }

    function hold(bytes32 id, uint16 index, bytes calldata seal) external nonReentrant {
        Order storage o = _order(id);
        Window storage w = _window(o, index);
        if (w.phase != Phase.Funded || !_currentKey(o.terms)) revert State();
        if (block.timestamp > o.terms.holdDeadline) revert Deadline();
        _verify(seal, _statement(o.terms, w.spec, 3));
        w.phase = Phase.Held;
        emit WindowUpdated(id, index, Phase.Held);
    }

    function publish(bytes32 id, uint16 index, bytes calldata capsule) external nonReentrant {
        Order storage o = _order(id);
        Window storage w = _window(o, index);
        if (w.phase != Phase.Held) revert State();
        if (block.timestamp > o.terms.keyDeadline) revert Deadline();
        if (capsule.length != 80 || sha256(capsule) != w.spec.capsuleHash) revert Invalid();
        w.capsule = capsule;
        w.phase = Phase.KeyPublished;
        if (o.terms.acceptance == 0) _earn(o, w);
        emit WindowUpdated(id, index, w.phase);
    }

    function open(bytes32 id, uint16 index, bytes calldata seal) external nonReentrant {
        Order storage o = _order(id);
        Window storage w = _window(o, index);
        if (w.decryptionProven) revert State();
        if (o.terms.acceptance == 1) {
            if (w.phase != Phase.KeyPublished) revert State();
            if (block.timestamp > o.terms.openDeadline) revert Deadline();
        } else if (w.phase != Phase.Earned) revert State();
        _verify(seal, _statement(o.terms, w.spec, 4));
        w.decryptionProven = true;
        if (o.terms.acceptance == 1) _earn(o, w);
        emit DecryptionProven(id, index);
        emit WindowUpdated(id, index, w.phase);
    }

    function withdraw(bytes32 id) external nonReentrant {
        Order storage o = _order(id);
        uint256 amount = o.earned - o.withdrawn;
        if (amount == 0) revert State();
        o.withdrawn = o.earned;
        _payout(o, o.terms.providerWallet, amount);
        emit Withdrawn(id, o.terms.providerWallet, amount);
    }

    function refund(bytes32 id, uint16 index) external nonReentrant {
        Order storage o = _order(id);
        Window storage w = _window(o, index);
        if (w.phase == Phase.Funded) {
            if (block.timestamp <= o.terms.holdDeadline && _currentKey(o.terms)) revert Deadline();
        } else if (w.phase == Phase.Held) {
            if (block.timestamp <= o.terms.keyDeadline) revert Deadline();
        } else if (w.phase == Phase.KeyPublished && o.terms.acceptance == 1) {
            if (block.timestamp <= o.terms.openDeadline) revert Deadline();
        } else revert State();
        w.phase = Phase.Refunded;
        ++o.refundedWindows;
        uint256 amount = w.spec.deliveryFee + w.spec.reservationFee + _takeRefundableBonus(o);
        o.refunded += amount;
        _payout(o, o.terms.payer, amount);
        emit WindowUpdated(id, index, Phase.Refunded);
        emit Refunded(id, o.terms.payer, amount);
    }

    function refundBonus(bytes32 id) external nonReentrant {
        Order storage o = _order(id);
        if (o.bonusEarned || o.bonusRefunded || (o.refundedWindows == 0 && block.timestamp <= o.terms.priorityDeadline)) revert State();
        uint256 amount = _takeRefundableBonus(o);
        o.refunded += amount;
        _payout(o, o.terms.payer, amount);
        emit Refunded(id, o.terms.payer, amount);
    }

    function cancel(bytes32 id) external nonReentrant {
        Order storage o = _order(id);
        if (msg.sender != o.terms.payer) revert Invalid();
        uint256 amount;
        uint16 count;
        for (uint16 i; i < o.windows.length; ++i) {
            Window storage w = o.windows[i];
            if (w.phase == Phase.Funded) {
                w.phase = Phase.Refunded;
                amount += w.spec.deliveryFee + w.spec.reservationFee;
                ++count;
                emit WindowUpdated(id, i, Phase.Refunded);
            } else if (w.phase != Phase.Refunded) revert State();
        }
        if (count == 0) revert State();
        o.refundedWindows += count;
        amount += _takeRefundableBonus(o);
        o.refunded += amount;
        _payout(o, o.terms.payer, amount);
        emit Refunded(id, o.terms.payer, amount);
    }

    function quoteWalletDigest(bytes32 digest, bytes32 providerPeer) public view returns (bytes32) {
        bytes32 separator = keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256("VibefaceDelivery"), keccak256("1"), block.chainid, address(this)
        ));
        return keccak256(abi.encodePacked(hex"1901", separator, keccak256(abi.encode(QUOTE_TYPEHASH, digest, providerPeer))));
    }

    function _terms(DeliveryStatement calldata t, WindowSpec[] calldata specs) private view returns (DeliveryStatement memory b) {
        if (t.relation != 2 || t.receiverPeer == 0 || t.receiverPublic == 0 || t.receiverKeyId == 0 || t.receiverKeySerial == 0
            || t.sourcePeer == 0 || t.providerPeer == 0 || t.payer == address(0) || t.providerWallet == address(0)
            || t.providerWallet == address(this) || t.asset.code.length == 0 || t.acceptance > 1
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

    function _payout(Order storage o, address destination, uint256 amount) private {
        if (_decimals(o.terms.asset) != o.assetDecimals) revert Token();
        _move(o.terms.asset, address(this), destination, amount, false);
        if (_decimals(o.terms.asset) != o.assetDecimals) revert Token();
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
