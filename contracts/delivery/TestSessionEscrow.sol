// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {SessionEscrow, ISessionToken} from "./SessionEscrow.sol";

// Accounting only. The digest used here is not a proof.
contract TestSessionEscrow is SessionEscrow {
    constructor(ISessionToken token, bytes32 image) SessionEscrow(token, image) {}

    function _verify(bytes calldata seal, bytes32 journal) internal pure override {
        if (seal.length != 32 || bytes32(seal) != journal) revert Proof();
    }
}
