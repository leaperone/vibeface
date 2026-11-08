// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

// Local settlement experiment. A receiver receipt is checked by the issuing Node, not by this contract.
contract CumulativeChannel {
    uint64 public constant CHALLENGE_WINDOW = 3600;
    bytes32 private constant VOUCHER_TYPEHASH = keccak256("CumulativeVoucher(bytes32 channelId,uint64 sequence,uint256 totalAmount,bytes32 receiptCommitment)");
    uint256 private constant HALF_CURVE_ORDER = 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;
    address public immutable asset;

    enum ChannelState { Open, Closing, Closed }
    struct Channel {
        address payer;
        address payee;
        bytes32 providerPeer;
        uint256 budget;
        uint256 redeemed;
        uint64 sequence;
        uint64 closesAt;
        ChannelState state;
    }
    mapping(bytes32 => Channel) private channels;
    uint256 private entered;

    error Invalid();
    error State();
    error Deadline();
    error Signature();
    error Token();
    error Reentrancy();

    event ChannelOpened(bytes32 indexed channelId, address indexed payer, address indexed payee, bytes32 providerPeer, uint256 budget);
    event Redeemed(bytes32 indexed channelId, uint64 sequence, uint256 totalAmount, bytes32 receiptCommitment);
    event Closing(bytes32 indexed channelId, uint64 closesAt);
    event Closed(bytes32 indexed channelId, uint256 refunded);

    modifier nonReentrant() {
        if (entered != 0) revert Reentrancy();
        entered = 1;
        _;
        entered = 0;
    }

    constructor(address token) {
        if (token == address(0) || token.code.length == 0) revert Invalid();
        asset = token;
        _decimals();
    }

    function getChannel(bytes32 id) external view returns (Channel memory) { return _channel(id); }

    function channelId(address payer, bytes32 salt) public view returns (bytes32) {
        return keccak256(abi.encode(block.chainid, address(this), payer, salt));
    }

    function openChannel(bytes32 salt, address payee, bytes32 providerPeer, uint256 budget) external nonReentrant returns (bytes32 id) {
        if (salt == 0 || payee == address(0) || payee == address(this) || payee == msg.sender || providerPeer == 0 || budget == 0) revert Invalid();
        id = channelId(msg.sender, salt);
        if (channels[id].payer != address(0)) revert State();
        channels[id] = Channel(msg.sender, payee, providerPeer, budget, 0, 0, 0, ChannelState.Open);
        _transfer(msg.sender, address(this), budget, true);
        emit ChannelOpened(id, msg.sender, payee, providerPeer, budget);
    }

    function voucherDigest(bytes32 id, uint64 sequence, uint256 totalAmount, bytes32 receiptCommitment) public view returns (bytes32) {
        bytes32 domain = keccak256(abi.encode(
            keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
            keccak256("VibefaceCumulativeChannel"), keccak256("1"), block.chainid, address(this)
        ));
        return keccak256(abi.encodePacked("\x19\x01", domain,
            keccak256(abi.encode(VOUCHER_TYPEHASH, id, sequence, totalAmount, receiptCommitment))));
    }

    function redeemChannel(bytes32 id, uint64 sequence, uint256 totalAmount, bytes32 receiptCommitment, bytes calldata signature) external nonReentrant {
        Channel storage channel = _channel(id);
        if (channel.state == ChannelState.Closed) revert State();
        if (channel.state == ChannelState.Closing && block.timestamp >= channel.closesAt) revert Deadline();
        if (sequence <= channel.sequence || totalAmount <= channel.redeemed || totalAmount > channel.budget || receiptCommitment == 0) revert Invalid();
        if (signature.length != 65) revert Signature();
        bytes32 r; bytes32 s; uint8 v;
        assembly { r := calldataload(signature.offset) s := calldataload(add(signature.offset, 32)) v := byte(0, calldataload(add(signature.offset, 64))) }
        if (uint256(s) > HALF_CURVE_ORDER || uint256(s) == 0 || (v != 27 && v != 28)
            || ecrecover(voucherDigest(id, sequence, totalAmount, receiptCommitment), v, r, s) != channel.payer) revert Signature();
        uint256 delta = totalAmount - channel.redeemed;
        channel.sequence = sequence;
        channel.redeemed = totalAmount;
        _transfer(address(this), channel.payee, delta, false);
        emit Redeemed(id, sequence, totalAmount, receiptCommitment);
    }

    function closeChannel(bytes32 id) external nonReentrant {
        Channel storage channel = _channel(id);
        if (msg.sender != channel.payer || channel.state != ChannelState.Open) revert State();
        channel.state = ChannelState.Closing;
        channel.closesAt = uint64(block.timestamp) + CHALLENGE_WINDOW;
        emit Closing(id, channel.closesAt);
    }

    function refundChannel(bytes32 id) external nonReentrant {
        Channel storage channel = _channel(id);
        if (channel.state != ChannelState.Closing) revert State();
        if (block.timestamp < channel.closesAt) revert Deadline();
        uint256 remainder = channel.budget - channel.redeemed;
        channel.state = ChannelState.Closed;
        if (remainder != 0) _transfer(address(this), channel.payer, remainder, false);
        emit Closed(id, remainder);
    }

    function _channel(bytes32 id) private view returns (Channel storage channel) {
        channel = channels[id];
        if (channel.payer == address(0)) revert Invalid();
    }

    function _decimals() private view {
        (bool ok, bytes memory value) = asset.staticcall(abi.encodeWithSignature("decimals()"));
        if (!ok || value.length != 32 || abi.decode(value, (uint256)) != 6) revert Token();
    }

    function _balance(address owner) private view returns (uint256) {
        (bool ok, bytes memory value) = asset.staticcall(abi.encodeWithSignature("balanceOf(address)", owner));
        if (!ok || value.length != 32) revert Token();
        return abi.decode(value, (uint256));
    }

    function _transfer(address from, address to, uint256 amount, bool deposit) private {
        _decimals();
        uint256 beforeFrom = _balance(from);
        uint256 beforeTo = _balance(to);
        (bool ok, bytes memory value) = asset.call(deposit
            ? abi.encodeWithSignature("transferFrom(address,address,uint256)", from, to, amount)
            : abi.encodeWithSignature("transfer(address,uint256)", to, amount));
        if (!ok || (value.length != 0 && (value.length != 32 || abi.decode(value, (uint256)) != 1))) revert Token();
        if (beforeFrom < amount || _balance(from) != beforeFrom - amount || _balance(to) != beforeTo + amount) revert Token();
        _decimals();
    }
}
