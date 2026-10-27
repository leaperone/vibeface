// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {DeliveryEscrow} from "./DeliveryEscrow.sol";
import {DeliveryStatement, DeliveryProfile} from "../../crates/delivery/abi/Statement.sol";

// Accounting tests only. A digest is deliberately not a ZK receipt.
contract AccountingTestEscrow is DeliveryEscrow {
    constructor(DeliveryProfile memory p) DeliveryEscrow(p) {}
    function previewRelation(DeliveryStatement calldata input, WindowSpec[] calldata specs, uint16 index, uint8 relation)
        external view returns (DeliveryStatement memory)
    {
        require(index < specs.length && relation >= 2 && relation <= 4, "preview range");
        DeliveryStatement memory statement = this.preview(input, specs)[index];
        statement.relation = relation;
        return statement;
    }
    function _verify(bytes calldata seal, DeliveryStatement memory statement) internal pure override {
        if (seal.length != 32 || bytes32(seal) != sha256(abi.encode(statement))) revert Proof();
    }
}

contract TestToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    uint8 public decimals = 6;
    uint8 public mode;
    address public target;
    bytes public payload;
    bool public reentrySucceeded;

    constructor(address owner) { balanceOf[owner] = 1000000000000; }
    function configure(uint8 nextMode) external { mode = nextMode; }
    function setDecimals(uint8 value) external { decimals = value; }
    function attack(address destination, bytes calldata data) external { target = destination; payload = data; }
    function approve(address spender, uint256 amount) external returns (bool) { allowance[msg.sender][spender] = amount; return true; }
    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        require(allowance[from][msg.sender] >= amount, "allowance");
        allowance[from][msg.sender] -= amount;
        return _transfer(from, to, amount);
    }
    function transfer(address to, uint256 amount) external returns (bool) { return _transfer(msg.sender, to, amount); }
    function _transfer(address from, address to, uint256 amount) private returns (bool) {
        require(mode != 3, "frozen");
        if (mode == 2) return false;
        uint256 debit = amount + (mode == 6 ? 1 : 0);
        require(balanceOf[from] >= debit, "balance");
        balanceOf[from] -= debit;
        balanceOf[to] += mode == 4 ? amount - 1 : amount;
        if (mode == 5) (reentrySucceeded,) = target.call(payload);
        if (mode == 7) decimals = 18;
        if (mode == 1) assembly { return(0, 0) }
        return true;
    }
}
