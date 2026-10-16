// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {BlobDAPoC, BeaconHeader, BlobItem, IZiskVerifier} from "../src/BlobDAPoC.sol";

/// Returns `root` for exactly one timestamp and reverts otherwise, like the real 4788 contract.
contract MockBeaconRoots {
    uint256 immutable ts;
    bytes32 immutable root;

    constructor(uint256 _ts, bytes32 _root) {
        ts = _ts;
        root = _root;
    }

    fallback(bytes calldata data) external returns (bytes memory) {
        require(data.length == 32 && abi.decode(data, (uint256)) == ts, "no root");
        return abi.encode(root);
    }
}

contract MockVerifier is IZiskVerifier {
    bool public called;
    bool public shouldRevert;

    function set(bool r) external {
        shouldRevert = r;
    }

    function verifySnarkProof(bytes32, bytes32, bytes calldata, bytes calldata) external view {
        require(!shouldRevert, "bad proof");
    }
}

contract BlobDAPoCTest is Test {
    address constant BEACON_ROOTS = 0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02;
    uint256 constant T = 1_800_000_000;

    BlobDAPoC poc;
    MockVerifier verifier;
    string sidecar;
    string fx;
    bytes32 blockRoot;
    BeaconHeader hdr;
    bytes32[17] branch;
    bytes com;
    uint64 index;
    bytes31 pid;
    bytes32 rootBefore;

    function setUp() public {
        sidecar = vm.readFile("../fixtures/beacon_sidecar.json");
        fx = vm.readFile("../fixtures/settle_fixture.json");
        blockRoot = vm.parseJsonBytes32(sidecar, ".block_root");
        hdr = BeaconHeader({
            slot: uint64(vm.parseJsonUint(sidecar, ".header.slot")),
            proposerIndex: uint64(vm.parseJsonUint(sidecar, ".header.proposer_index")),
            parentRoot: vm.parseJsonBytes32(sidecar, ".header.parent_root"),
            stateRoot: vm.parseJsonBytes32(sidecar, ".header.state_root"),
            bodyRoot: vm.parseJsonBytes32(sidecar, ".header.body_root")
        });
        bytes32[] memory b = vm.parseJsonBytes32Array(sidecar, ".kzg_commitment_inclusion_proof");
        require(b.length == 17, "branch");
        for (uint256 i = 0; i < 17; ++i) {
            branch[i] = b[i];
        }
        com = vm.parseJsonBytes(sidecar, ".kzg_commitment");
        index = uint64(vm.parseJsonUint(sidecar, ".index"));

        pid = bytes31(bytes32(vm.parseJsonBytes(fx, ".pid")));
        rootBefore = vm.parseJsonBytes32(fx, ".root_before");
        verifier = new MockVerifier();
        poc = new BlobDAPoC(verifier, bytes32(uint256(0xaa)), bytes32(uint256(0xbb)));
        vm.etch(BEACON_ROOTS, address(new MockBeaconRoots(T, blockRoot)).code);
    }

    // ------------------------------------------------------------ helpers

    function _item() internal view returns (BlobItem[] memory items) {
        items = new BlobItem[](1);
        items[0] = BlobItem({
            commitment: com,
            y: vm.parseJsonBytes32(fx, ".y"),
            kzgProof: vm.parseJsonBytes(fx, ".kzg_proof"),
            index: index,
            branch: branch
        });
    }

    function _settle(BeaconHeader memory h, uint256 t, BlobItem[] memory items) internal returns (bytes32) {
        return poc.settle(pid, rootBefore, bytes32(uint256(3)), h, t, items, hex"", hex"01");
    }

    // ------------------------------------------------------------ SSZ

    function test_headerRoot() public view {
        assertEq(poc.headerRoot(hdr), blockRoot);
    }

    function test_headerRootChangesWithEveryField() public view {
        BeaconHeader memory h = hdr;
        h.slot += 1;
        assertTrue(poc.headerRoot(h) != blockRoot);
        h = hdr;
        h.proposerIndex += 1;
        assertTrue(poc.headerRoot(h) != blockRoot);
        h = hdr;
        h.parentRoot = bytes32(uint256(h.parentRoot) ^ 1);
        assertTrue(poc.headerRoot(h) != blockRoot);
    }

    function test_commitmentRoot() public view {
        assertEq(poc.commitmentRoot(com, index, branch), hdr.bodyRoot);
    }

    function test_commitmentRootFlippedBranchByte() public view {
        for (uint256 i = 0; i < 17; ++i) {
            bytes32[17] memory b = branch;
            b[i] = bytes32(uint256(b[i]) ^ 1);
            assertTrue(poc.commitmentRoot(com, index, b) != hdr.bodyRoot, "flip");
        }
    }

    function test_commitmentRootWrongIndex() public view {
        assertTrue(poc.commitmentRoot(com, index + 1, branch) != hdr.bodyRoot);
        assertTrue(poc.commitmentRoot(com, 0, branch) != hdr.bodyRoot);
    }

    // ------------------------------------------------------------ shared vector with the Rust driver

    /// Same vector as driver/src/da.rs (z_and_digest_vector).
    function test_sharedVector() public pure {
        bytes memory c = new bytes(48);
        for (uint8 i = 0; i < 48; ++i) {
            c[i] = bytes1(i);
        }
        bytes32 z = bytes32(
            uint256(sha256(abi.encodePacked(bytes32(uint256(1)), bytes32(uint256(2)), c)))
                % 52435875175126190479447740508185965837690552500527637822603658699938581184513
        );
        bytes32 digest = sha256(abi.encodePacked(c, bytes32(uint256(0x0707070707070707070707070707070707070707070707070707070707070707))));
        assertEq(z, SHARED_Z);
        assertEq(digest, SHARED_DIGEST);
    }

    bytes32 constant SHARED_Z = 0x6b583eea0ff2e858075e8d69bd663f0b1d30588f84d4720bd03811c05bdff939;
    bytes32 constant SHARED_DIGEST = 0xc0767f3eb21214f8e61103382318931dd6a728d70fd5de3179350bf99b59c31f;

    // ------------------------------------------------------------ settle

    function test_settleEndToEnd() public {
        // exercises the real 0x0A precompile with the real mainnet blob commitment
        vm.warp(1234);
        vm.recordLogs();
        bytes32 digest = _settle(hdr, T, _item());
        assertEq(digest, vm.parseJsonBytes32(fx, ".digest"));
        assertEq(poc.latestRoot(pid), bytes32(uint256(3)));
        assertEq(poc.batches(pid), 1);
        assertEq(poc.lastAt(pid), 1234);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        Vm.Log memory l = logs[logs.length - 1];
        assertEq(l.topics[0], BlobDAPoC.Settled.selector);
        (bytes32 rb, bytes32 ra, bytes32 br, bytes32 d, uint256 n) =
            abi.decode(l.data, (bytes32, bytes32, bytes32, bytes32, uint256));
        assertEq(rb, rootBefore);
        assertEq(ra, bytes32(uint256(3)));
        assertEq(br, blockRoot);
        assertEq(d, digest);
        assertEq(n, 1);
    }

    /// The Rust driver's ABI encoding of settle() is accepted as is.
    function test_driverCalldata() public {
        bytes memory cd = vm.parseJsonBytes(fx, ".settle_calldata");
        (bool ok,) = address(poc).call(cd);
        assertTrue(ok);
        assertEq(poc.batches(pid), 1);
        assertEq(poc.latestRoot(pid), bytes32(uint256(3)));
    }

    function test_precompileRunsKzg() public view {
        // the precompile must reject a corrupted y: proves 0x0A really verifies in this EVM
        bytes32 z = vm.parseJsonBytes32(fx, ".z");
        bytes32 y = vm.parseJsonBytes32(fx, ".y");
        bytes memory proof = vm.parseJsonBytes(fx, ".kzg_proof");
        bytes32 vh = bytes32((uint256(sha256(com)) & (type(uint256).max >> 8)) | (uint256(1) << 248));
        (bool ok, bytes memory out) = address(0x0a).staticcall(abi.encodePacked(vh, z, y, com, proof));
        assertTrue(ok);
        assertEq(out.length, 64);
        (bool bad,) = address(0x0a).staticcall(abi.encodePacked(vh, z, bytes32(uint256(y) ^ 1), com, proof));
        assertFalse(bad);
    }

    function test_settleWithoutProofSkipsVerifier() public {
        verifier.set(true);
        poc.settle(pid, rootBefore, bytes32(uint256(3)), hdr, T, _item(), hex"", hex"");
        assertEq(poc.batches(pid), 1);
    }

    function test_settleRevertsOnBadProof() public {
        verifier.set(true);
        BlobItem[] memory items = _item();
        vm.expectRevert("bad proof");
        _settle(hdr, T, items);
    }

    function test_revertWrongTimestamp() public {
        BlobItem[] memory items = _item();
        vm.expectRevert(BlobDAPoC.BeaconRootUnavailable.selector);
        _settle(hdr, T + 2, items);
    }

    function test_revertWrongHeader() public {
        BeaconHeader memory h = hdr;
        h.slot += 1;
        BlobItem[] memory items = _item();
        vm.expectRevert(BlobDAPoC.HeaderMismatch.selector);
        _settle(h, T, items);
    }

    function test_revertWrongY() public {
        BlobItem[] memory items = _item();
        items[0].y = bytes32(uint256(items[0].y) ^ 1);
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.InvalidBlobOpening.selector, 0));
        _settle(hdr, T, items);
    }

    function test_revertWrongCommitment() public {
        BlobItem[] memory items = _item();
        bytes memory c = items[0].commitment;
        c[5] = bytes1(uint8(c[5]) ^ 1);
        items[0].commitment = c;
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.InclusionMismatch.selector, 0));
        _settle(hdr, T, items);
    }

    function test_revertWrongIndex() public {
        BlobItem[] memory items = _item();
        items[0].index = index + 1;
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.InclusionMismatch.selector, 0));
        _settle(hdr, T, items);
        items[0].index = 4096;
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.BadIndex.selector, 0));
        _settle(hdr, T, items);
    }

    function test_revertWrongProofOrRootBefore() public {
        BlobItem[] memory items = _item();
        // a different rootBefore changes z, so the opening no longer verifies
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.InvalidBlobOpening.selector, 0));
        poc.settle(pid, bytes32(uint256(9)), bytes32(uint256(3)), hdr, T, items, hex"", hex"");
        items[0].kzgProof = new bytes(47);
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.BadProofLength.selector, 0));
        _settle(hdr, T, items);
    }

    function test_revertNoBlobs() public {
        BlobItem[] memory items = new BlobItem[](0);
        vm.expectRevert(BlobDAPoC.NoBlobs.selector);
        _settle(hdr, T, items);
    }

    function test_revertBadCommitmentLength() public {
        BlobItem[] memory items = _item();
        items[0].commitment = new bytes(47);
        vm.expectRevert(abi.encodeWithSelector(BlobDAPoC.BadCommitmentLength.selector, 0));
        _settle(hdr, T, items);
    }

    function test_revertWhenNo4788() public {
        vm.etch(BEACON_ROOTS, hex"");
        BlobItem[] memory items = _item();
        vm.expectRevert(BlobDAPoC.BeaconRootUnavailable.selector);
        _settle(hdr, T, items);
    }
}
