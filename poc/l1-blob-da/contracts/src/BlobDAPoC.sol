// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

interface IZiskVerifier {
    function verifySnarkProof(bytes32, bytes32, bytes calldata, bytes calldata) external view;
}

struct BeaconHeader {
    uint64 slot;
    uint64 proposerIndex;
    bytes32 parentRoot;
    bytes32 stateRoot;
    bytes32 bodyRoot;
}

struct BlobItem {
    bytes commitment; // 48 bytes
    bytes32 y;
    bytes kzgProof; // 48 bytes
    uint64 index; // position of the commitment in the beacon block body
    bytes32[17] branch; // SSZ inclusion proof of the commitment in the body root
}

/// @notice PoC: DAVINCI blob data published on Ethereum L1, proven on OP through the EIP-4788 beacon root.
/// @dev The L1 blob is never visible to the EVM on OP. Instead the beacon block that carried the blob is
/// authenticated (header -> block root == 4788(l2Timestamp)), the commitment is proven to sit in that block's
/// body (SSZ merkle branch), and the 0x0A point-evaluation precompile opens it at the DAVINCI challenge z.
contract BlobDAPoC {
    address internal constant BEACON_ROOTS = 0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02;
    address internal constant POINT_EVALUATION = address(0x0A);
    uint256 internal constant BLS_MODULUS =
        52435875175126190479447740508185965837690552500527637822603658699938581184513;
    uint256 internal constant FIELD_ELEMENTS_PER_BLOB = 4096;
    uint256 internal constant MAX_BLOB_COMMITMENTS_PER_BLOCK = 4096;
    /// generalized index of body.blob_kzg_commitments[0]: field 11 of 16 (gindex 27), list of 4096 + length mixin.
    uint256 internal constant COMMITMENTS_GINDEX_BASE = 27 * 8192;

    IZiskVerifier public immutable verifier;
    bytes32 public immutable programVK;
    bytes32 public immutable rootC;

    mapping(bytes31 => bytes32) public latestRoot;
    mapping(bytes31 => uint256) public batches;
    mapping(bytes31 => uint256) public lastAt;

    event Settled(
        bytes31 indexed processId,
        bytes32 rootBefore,
        bytes32 rootAfter,
        bytes32 beaconRoot,
        bytes32 blobsDigest,
        uint256 nBlobs
    );

    error BeaconRootUnavailable();
    error HeaderMismatch();
    error NoBlobs();
    error BadCommitmentLength(uint256 i);
    error BadProofLength(uint256 i);
    error BadIndex(uint256 i);
    error InclusionMismatch(uint256 i);
    error InvalidBlobOpening(uint256 i);

    constructor(IZiskVerifier _verifier, bytes32 _programVK, bytes32 _rootC) {
        verifier = _verifier;
        programVK = _programVK;
        rootC = _rootC;
    }

    function settle(
        bytes31 pid,
        bytes32 rootBefore,
        bytes32 rootAfter,
        BeaconHeader calldata h,
        uint256 l2Timestamp,
        BlobItem[] calldata blobs,
        bytes calldata publicValues,
        bytes calldata proofBytes
    ) external returns (bytes32 digest) {
        // (1) the beacon root of the L1 origin of the OP block at l2Timestamp
        (bool ok, bytes memory ret) = BEACON_ROOTS.staticcall(abi.encode(l2Timestamp));
        if (!ok || ret.length != 32) revert BeaconRootUnavailable();
        bytes32 beaconRoot = abi.decode(ret, (bytes32));

        // (2) the header hashes to that root
        if (headerRoot(h) != beaconRoot) revert HeaderMismatch();

        // (3) every commitment is in the block body and opens at z to y
        uint256 n = blobs.length;
        if (n == 0) revert NoBlobs();
        bytes32 pidBE = bytes32(uint256(uint248(pid)));
        bytes memory pairs;
        for (uint256 i = 0; i < n; ++i) {
            BlobItem calldata b = blobs[i];
            if (b.commitment.length != 48) revert BadCommitmentLength(i);
            if (b.kzgProof.length != 48) revert BadProofLength(i);
            if (b.index >= MAX_BLOB_COMMITMENTS_PER_BLOCK) revert BadIndex(i);
            if (commitmentRoot(b.commitment, b.index, b.branch) != h.bodyRoot) revert InclusionMismatch(i);

            bytes32 vh = bytes32((uint256(sha256(b.commitment)) & type(uint256).max >> 8) | (uint256(1) << 248));
            bytes32 z = bytes32(uint256(sha256(abi.encodePacked(pidBE, rootBefore, b.commitment))) % BLS_MODULUS);
            (bool okp, bytes memory out) =
                POINT_EVALUATION.staticcall(abi.encodePacked(vh, z, b.y, b.commitment, b.kzgProof));
            if (!okp || out.length != 64) revert InvalidBlobOpening(i);
            (uint256 fe, uint256 mod) = abi.decode(out, (uint256, uint256));
            if (fe != FIELD_ELEMENTS_PER_BLOB || mod != BLS_MODULUS) revert InvalidBlobOpening(i);
            pairs = bytes.concat(pairs, b.commitment, b.y);
        }

        // (4) the digest the zkVM proof publishes
        digest = sha256(pairs);

        // (5) the proof (skippable in measurements)
        if (proofBytes.length > 0) verifier.verifySnarkProof(programVK, rootC, publicValues, proofBytes);

        // (6) registry-like writes
        latestRoot[pid] = rootAfter;
        ++batches[pid];
        lastAt[pid] = block.timestamp;
        emit Settled(pid, rootBefore, rootAfter, beaconRoot, digest, n);
    }

    /// @notice SSZ hash_tree_root of a BeaconBlockHeader (== the beacon block root).
    function headerRoot(BeaconHeader calldata h) public pure returns (bytes32) {
        bytes32 l0 = _u64le(h.slot);
        bytes32 l1 = _u64le(h.proposerIndex);
        bytes32 n01 = sha256(abi.encodePacked(l0, l1));
        bytes32 n23 = sha256(abi.encodePacked(h.parentRoot, h.stateRoot));
        bytes32 n45 = sha256(abi.encodePacked(h.bodyRoot, bytes32(0)));
        bytes32 n67 = sha256(abi.encodePacked(bytes32(0), bytes32(0)));
        return sha256(abi.encodePacked(sha256(abi.encodePacked(n01, n23)), sha256(abi.encodePacked(n45, n67))));
    }

    /// @notice Folds the SSZ branch of blob_kzg_commitments[index]; returns the body root it leads to.
    function commitmentRoot(bytes calldata com, uint64 index, bytes32[17] calldata branch)
        public
        pure
        returns (bytes32 node)
    {
        // a 48-byte commitment is chunked as 32 + 16 bytes (zero-padded), hashed to one leaf
        node = sha256(abi.encodePacked(com, bytes16(0)));
        uint256 g = COMMITMENTS_GINDEX_BASE + index;
        for (uint256 i = 0; i < 17; ++i) {
            node = (g >> i) & 1 == 1 ? sha256(abi.encodePacked(branch[i], node)) : sha256(abi.encodePacked(node, branch[i]));
        }
    }

    function _u64le(uint64 v) private pure returns (bytes32 r) {
        uint256 x = v;
        uint256 le;
        for (uint256 i = 0; i < 8; ++i) {
            le = (le << 8) | (x & 0xff);
            x >>= 8;
        }
        // the 8 little-endian bytes sit in the most significant bytes
        r = bytes32(le << 192);
    }
}
