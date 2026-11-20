// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {RiscZeroAdapter} from "./RiscZeroAdapter.sol";

interface ISessionToken {
    function balanceOf(address owner) external view returns (uint256);
    function decimals() external view returns (uint8);
}

contract SessionEscrow {
    error Invalid();
    error Unauthorized();
    error Proof();
    error Token();

    uint256 public constant MAX_BATCH = 16;
    uint256 public constant MAX_CLAIM_MS = 7 days * 1000;
    bytes32 public immutable deployment;
    RiscZeroAdapter public immutable adapter;
    ISessionToken public immutable asset;
    bool private entered;

    struct Budget {
        address payer;
        address payee;
        bytes32 relay;
        uint256 available;
        uint256 credit;
        bool closed;
    }

    struct Reservation {
        bytes32 commitment;
        uint256 amount;
        uint64 claimUntilMs;
        bool consumed;
    }

    struct Terms {
        bytes32 delivery;
        bytes32 receiptContext;
        uint256 amount;
        uint64 claimUntilMs;
    }

    mapping(bytes32 => Budget) public budgets;
    mapping(bytes32 => mapping(bytes32 => Reservation)) public reservations;
    event BudgetOpened(bytes32 indexed budgetId, address payer, address payee, uint256 amount);
    event Reserved(bytes32 indexed budgetId, bytes32 indexed orderId, bytes32 commitment);
    event Settled(bytes32 indexed budgetId, bytes32 indexed ordersHash, uint256 amount);
    event Refunded(bytes32 indexed budgetId, uint256 amount);
    event Withdrawn(bytes32 indexed budgetId, uint256 amount);

    modifier nonReentrant() {
        if (entered) revert Invalid();
        entered = true;
        _;
        entered = false;
    }

    constructor(ISessionToken token, bytes32 image) {
        if (address(token).code.length == 0 || token.decimals() != 6) revert Token();
        asset = token;
        deployment = sha256(abi.encode(sha256("vibeface-session-escrow-evm-v1"), block.chainid, address(this)));
        adapter = new RiscZeroAdapter(image);
    }

    function openBudget(bytes32 salt, address payee, bytes32 relay, uint256 deposit)
        external nonReentrant returns (bytes32 id)
    {
        if (salt == 0 || relay == 0 || deposit == 0 || payee == address(0)
            || payee == address(this) || payee == msg.sender) revert Invalid();
        id = sha256(abi.encode(sha256("vibeface-session-budget-v1"), deployment, msg.sender, salt));
        if (budgets[id].payer != address(0)) revert Invalid();
        budgets[id] = Budget(msg.sender, payee, relay, deposit, 0, false);
        _move(msg.sender, address(this), deposit, true);
        emit BudgetOpened(id, msg.sender, payee, deposit);
    }

    function reserveBatch(bytes32 budgetId, Terms[] calldata terms) external nonReentrant {
        Budget storage budget = budgets[budgetId];
        if (budget.payer != msg.sender || budget.closed) revert Unauthorized();
        _checkSize(terms.length);
        uint256 nowMs = block.timestamp * 1000;
        for (uint256 i; i < terms.length; ++i) {
            Terms calldata term = terms[i];
            if (term.delivery == 0 || term.receiptContext == 0 || term.amount == 0
                || term.claimUntilMs <= nowMs || term.claimUntilMs > nowMs + MAX_CLAIM_MS) revert Invalid();
            bytes32 id = sha256(abi.encodePacked("vibeface-session-order-v1", deployment, budgetId, term.delivery));
            bytes32 commitment = sha256(abi.encodePacked(
                "vibeface-session-settlement-terms-v1", id, term.receiptContext, budget.relay,
                term.amount, term.claimUntilMs
            ));
            Reservation storage saved = reservations[budgetId][id];
            if (saved.commitment != 0) {
                if (saved.commitment != commitment) revert Invalid();
                continue;
            }
            if (budget.available < term.amount) revert Invalid();
            budget.available -= term.amount;
            reservations[budgetId][id] = Reservation(commitment, term.amount, term.claimUntilMs, false);
            emit Reserved(budgetId, id, commitment);
        }
    }

    function settleBatch(bytes32 budgetId, bytes32[] calldata ids, bytes calldata seal) external nonReentrant {
        _checkSize(ids.length);
        Budget storage budget = budgets[budgetId];
        bytes memory commitments = "vibeface-session-settlement-batch-v1";
        uint64 earliest = type(uint64).max;
        uint256 total;
        uint256 nowMs = block.timestamp * 1000;
        bytes32 previous;
        for (uint256 i; i < ids.length; ++i) {
            bytes32 id = ids[i];
            Reservation storage item = reservations[budgetId][id];
            if (id <= previous || item.commitment == 0 || item.consumed || nowMs > item.claimUntilMs) revert Invalid();
            previous = id;
            item.consumed = true;
            commitments = bytes.concat(commitments, item.commitment);
            total += item.amount;
            if (item.claimUntilMs < earliest) earliest = item.claimUntilMs;
        }
        bytes32 ordersHash = sha256(commitments);
        bytes32 journal = sha256(abi.encode(
            sha256("vibeface-session-settlement-journal-v1"), deployment, budgetId, budget.relay,
            ordersHash, uint8(ids.length), total, earliest
        ));
        _verify(seal, journal);
        budget.credit += total;
        emit Settled(budgetId, ordersHash, total);
    }

    function withdraw(bytes32 budgetId) external nonReentrant {
        Budget storage budget = budgets[budgetId];
        uint256 amount = budget.credit;
        if (amount == 0) revert Invalid();
        budget.credit = 0;
        _move(address(this), budget.payee, amount, false);
        emit Withdrawn(budgetId, amount);
    }

    function closeBudget(bytes32 budgetId) external nonReentrant {
        Budget storage budget = budgets[budgetId];
        if (budget.payer != msg.sender || budget.closed) revert Unauthorized();
        budget.closed = true;
        uint256 amount = budget.available;
        budget.available = 0;
        if (amount != 0) _move(address(this), msg.sender, amount, false);
        emit Refunded(budgetId, amount);
    }

    function refundExpired(bytes32 budgetId, bytes32[] calldata ids) external nonReentrant {
        _checkSize(ids.length);
        uint256 amount;
        uint256 nowMs = block.timestamp * 1000;
        for (uint256 i; i < ids.length; ++i) {
            Reservation storage item = reservations[budgetId][ids[i]];
            if (item.commitment == 0 || item.consumed || nowMs <= item.claimUntilMs) revert Invalid();
            item.consumed = true;
            amount += item.amount;
        }
        _move(address(this), budgets[budgetId].payer, amount, false);
        emit Refunded(budgetId, amount);
    }

    function _verify(bytes calldata seal, bytes32 journal) internal view virtual {
        if (!adapter.verify(seal, journal)) revert Proof();
    }

    function _checkSize(uint256 length) private pure {
        if (length == 0 || length > MAX_BATCH) revert Invalid();
    }

    function _move(address from, address to, uint256 amount, bool incoming) private {
        if (asset.decimals() != 6) revert Token();
        uint256 fromBefore = asset.balanceOf(from);
        uint256 toBefore = asset.balanceOf(to);
        bytes memory data = incoming
            ? abi.encodeWithSignature("transferFrom(address,address,uint256)", from, to, amount)
            : abi.encodeWithSignature("transfer(address,uint256)", to, amount);
        (bool ok, bytes memory result) = address(asset).call(data);
        if (!ok || (result.length != 0 && (result.length != 32 || !abi.decode(result, (bool))))) revert Token();
        if (fromBefore < amount || asset.balanceOf(from) != fromBefore - amount
            || asset.balanceOf(to) != toBefore + amount || asset.decimals() != 6) revert Token();
    }
}
