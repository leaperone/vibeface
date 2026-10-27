// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {RiscZeroGroth16Verifier} from "risc0/contracts/src/groth16/RiscZeroGroth16Verifier.sol";

contract RiscZeroAdapter {
    bytes32 public constant CONTROL_ROOT = 0xa54dc85ac99f851c92d7c96d7318af41dbe7c0194edfcc37eb4d422a998c1f56;
    bytes32 public constant BN254_CONTROL_ID = 0x04446e66d300eb7fb45c9726bb53c793dda407a62e9601618bb43c5c14657ac0;
    bytes4 public constant SELECTOR = 0x73c457ba;
    RiscZeroGroth16Verifier public immutable verifier;
    bytes32 public immutable imageId;

    constructor(bytes32 guestImageId) {
        require(guestImageId != bytes32(0), "zero image");
        imageId = guestImageId;
        verifier = new RiscZeroGroth16Verifier(CONTROL_ROOT, BN254_CONTROL_ID);
        require(verifier.SELECTOR() == SELECTOR, "verifier parameters");
    }

    function verify(bytes calldata seal, bytes32 journalDigest) external view returns (bool) {
        try verifier.verify(seal, imageId, journalDigest) { return true; }
        catch { return false; }
    }
}
