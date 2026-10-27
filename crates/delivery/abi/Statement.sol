// SPDX-License-Identifier: MIT
pragma solidity >=0.8.20 <0.9.0;

struct DeliveryStatement {
    bytes32 domain;
    uint8 relation;
    uint256 chainId;
    address escrow;
    bytes32 profileId;
    bytes32 guestImageId;
    bytes32 orderId;
    bytes32 preTermsHash;
    bytes32 manifestRoot;
    bytes32 quoteDigest;
    bytes32 sourcePeer;
    bytes32 receiverPeer;
    bytes32 providerPeer;
    address payer;
    address providerWallet;
    address asset;
    uint8 acceptance;
    bytes32 receiverPublic;
    bytes32 receiverKeyId;
    uint64 receiverKeySerial;
    uint64 keyValidFrom;
    uint64 keyValidUntil;
    bytes32 keyBindingHash;
    uint8 cryptoSuite;
    uint64 totalPlaintextBytes;
    uint16 windowCount;
    uint16 windowIndex;
    uint64 windowOffset;
    uint32 windowPlaintextBytes;
    uint32 windowCiphertextBytes;
    bytes32 ciphertextHash;
    bytes32 capsuleHash;
    bytes32 plaintextCommitment;
    uint256 windowDeliveryFee;
    uint256 windowReservationFee;
    uint256 wholeOrderBonus;
    uint256 escrowAmount;
    uint64 quoteExpiresAt;
    uint64 holdDeadline;
    uint64 keyDeadline;
    uint64 openDeadline;
    uint64 retainUntil;
    uint64 priorityDeadline;
    uint32 keyPurpose;
    uint256 payerOrderNonce;
    bytes32 providerQuoteNonce;
    uint8 bindingAction;
}

struct DeliveryProfile {
    bytes32 domain;
    bytes32 guestImageId;
    bytes32 statementSchemaHash;
    uint8 cryptoSuite;
    uint16 maxWindows;
    uint32 maxWindowBytes;
    uint64 maxTotalBytes;
    uint64 maxGuestCycles;
    uint64 maxProverMemoryBytes;
}
