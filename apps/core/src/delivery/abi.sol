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
                struct ChainWindow { WindowSpec spec; uint8 phase; bool decryptionProven; bytes capsule; }
                struct ChainOrder {
            DeliveryStatement terms;
            ChainWindow[] windows;
            uint256 earned;
            uint256 withdrawn;
            uint256 refunded;
            uint16 earnedWindows;
            uint16 refundedWindows;
            uint8 assetDecimals;
            bool bonusEarned;
            bool bonusRefunded;
        }
        struct DeliveryQuote { bytes32 quoteDigest; bytes32 providerPeer; }
        function getProfile() external view returns (DeliveryProfile);
        function profileId() external view returns (bytes32);
        function adapter() external view returns (address);
        function verifier() external view returns (address);
        function imageId() external view returns (bytes32);
        function CONTROL_ROOT() external view returns (bytes32);
        function CONTROL_ROOT_0() external view returns (bytes16);
        function CONTROL_ROOT_1() external view returns (bytes16);
        function BN254_CONTROL_ID() external view returns (bytes32);
        function SELECTOR() external view returns (bytes4);
        function keys(bytes32 peer) external view returns (bytes32 bindingHash, uint64 serial, uint64 validFrom, uint64 validUntil, bool active);
        function getOrder(bytes32 id) external view returns (ChainOrder);
        function getWindow(bytes32 id, uint16 index) external view returns (ChainWindow);
        function publicValues(bytes32 id, uint16 index, uint8 relation) external view returns (DeliveryStatement);
        function registerKey(DeliveryStatement input, bytes seal) external;
        function open(bytes32 id, uint16 index, bytes seal) external;
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function decimals() external view returns (uint8);

struct ChainBudget {
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
struct Reservation { DeliveryStatement input; WindowSpec[] specs; bytes[] seals; bytes walletSignature; }
struct WindowRef { bytes32 orderId; uint16 index; }
struct HoldItem { bytes32 orderId; uint16 index; bytes seal; }
struct RevealItem { bytes32 orderId; uint16 index; bytes capsule; }
function getBudget(bytes32 id) external view returns (ChainBudget);
function orderBudget(bytes32 id) external view returns (bytes32);
function openBudget(bytes32 salt, address payee, bytes32 providerPeer, address asset, uint256 deposit) external returns (bytes32);
function reserveBatch(bytes32 budgetId, Reservation[] reservations) external;
function holdBatch(bytes32 budgetId, HoldItem[] items) external;
function revealBatch(bytes32 budgetId, RevealItem[] items) external;
function withdrawBudget(bytes32 id) external;
function closeBudget(bytes32 id) external;
function refundBudget(bytes32 id, WindowRef[] items) external;
