// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IBridge} from "./IBridge.sol";

interface ISignalService {
    function proveSignalReceived(
        uint64 _srcChainId, address _app, bytes32 _signal, bytes calldata _proof
    ) external returns (uint256);
}

interface IBridgeHasher {
    function hashMessage(IBridge.Message calldata _message) external pure returns (bytes32);
}

/// @notice Transient ProofStore V2: stores propose calldata + return proof data.
///         All data auto-clears after the transaction via EIP-1153 transient storage.
contract ProofStoreV2 {
    uint256 constant PROPOSE_TARGET_SLOT = 0;
    uint256 constant PROPOSE_LEN_SLOT = 1;
    uint256 constant PROPOSE_DATA_START = 100;
    uint256 constant RETURN_PROOF_LEN_SLOT = 11;
    uint256 constant RETURN_PROOF_DATA_START = 200000;
    uint256 constant RETURN_DATA_LEN_SLOT = 12;
    uint256 constant RETURN_DATA_START = 300000;
    uint256 constant RETURN_SUCCESS_SLOT = 13;
    uint256 constant HAS_DATA_SLOT = 14;
    uint256 constant STORED_CALL_ID_SLOT = 15;
    uint256 constant RETURN_MSG_ID_SLOT = 16;

    function store(
        uint256 callId,
        address proposeTarget,
        bytes calldata proposeCalldata,
        uint64 returnMsgId,
        bool returnSuccess,
        bytes calldata returnData,
        bytes calldata hopProof
    ) external {
        assembly {
            tstore(PROPOSE_TARGET_SLOT, proposeTarget)
            tstore(PROPOSE_LEN_SLOT, proposeCalldata.length)
            tstore(RETURN_SUCCESS_SLOT, returnSuccess)
            tstore(RETURN_DATA_LEN_SLOT, returnData.length)
            tstore(RETURN_PROOF_LEN_SLOT, hopProof.length)
            tstore(HAS_DATA_SLOT, 1)
            tstore(STORED_CALL_ID_SLOT, callId)
            tstore(RETURN_MSG_ID_SLOT, returnMsgId)
        }
        _tstoreBytes(PROPOSE_DATA_START, proposeCalldata);
        _tstoreBytes(RETURN_DATA_START, returnData);
        _tstoreBytes(RETURN_PROOF_DATA_START, hopProof);
    }

    function executePropose(uint256) external returns (bool) {
        address target; uint256 len;
        assembly {
            target := tload(PROPOSE_TARGET_SLOT)
            len := tload(PROPOSE_LEN_SLOT)
        }
        if (target == address(0)) return false;
        bytes memory data = _tloadBytes(PROPOSE_DATA_START, len);
        (bool ok,) = target.call(data);
        return ok;
    }

    function getReturnProof(uint256) external view
        returns (
            uint64 returnMsgId,
            bool success,
            bytes memory retData,
            bytes memory proof,
            uint256 storedCallId
        )
    {
        uint256 retLen; uint256 proofLen; uint256 hasData;
        assembly {
            hasData := tload(HAS_DATA_SLOT)
            returnMsgId := tload(RETURN_MSG_ID_SLOT)
            success := tload(RETURN_SUCCESS_SLOT)
            retLen := tload(RETURN_DATA_LEN_SLOT)
            proofLen := tload(RETURN_PROOF_LEN_SLOT)
            storedCallId := tload(STORED_CALL_ID_SLOT)
        }
        if (hasData == 0) return (0, false, "", "", 0);
        retData = _tloadBytes(RETURN_DATA_START, retLen);
        proof = _tloadBytes(RETURN_PROOF_DATA_START, proofLen);
    }

    function _tstoreBytes(uint256 startSlot, bytes calldata data) internal {
        uint256 words = (data.length + 31) / 32;
        for (uint256 i = 0; i < words; i++) {
            bytes32 word;
            uint256 offset = i * 32;
            if (offset + 32 <= data.length) {
                word = bytes32(data[offset:offset + 32]);
            } else {
                bytes memory buf = new bytes(32);
                uint256 remaining = data.length - offset;
                for (uint256 j = 0; j < remaining; j++) {
                    buf[j] = data[offset + j];
                }
                word = bytes32(buf);
            }
            assembly { tstore(add(startSlot, i), word) }
        }
    }

    function _tloadBytes(uint256 startSlot, uint256 len) internal view returns (bytes memory data) {
        data = new bytes(len);
        uint256 words = (len + 31) / 32;
        for (uint256 i = 0; i < words; i++) {
            bytes32 word;
            assembly { word := tload(add(startSlot, i)) }
            assembly { mstore(add(add(data, 32), mul(i, 32)), word) }
        }
    }
}

/// @notice Synchronous L1 proxy for L2 contracts. Verifies return values
///         cryptographically by reconstructing the return message onchain
///         and verifying its hash against a Merkle proof of L2 state.
///         A malicious builder cannot forge return data because the hash
///         is computed from the claimed data, not provided separately.
contract SyncL1ProxyV2 {
    address public immutable l2Target;
    address public immutable bridge;
    address public immutable l2Receiver;
    address public immutable l2Bridge;
    address public immutable signalService;
    address public immutable proofStore;
    uint64 public immutable destChainId;
    uint64 public immutable srcChainId;

    uint256 public callNonce;

    constructor(
        address _l2Target, address _bridge, address _l2Receiver,
        address _l2Bridge, address _signalService, address _proofStore,
        uint64 _destChainId, uint64 _srcChainId
    ) {
        l2Target = _l2Target;
        bridge = _bridge;
        l2Receiver = _l2Receiver;
        l2Bridge = _l2Bridge;
        signalService = _signalService;
        proofStore = _proofStore;
        destChainId = _destChainId;
        srcChainId = _srcChainId;
    }

    error ProofNotLoaded();

    fallback(bytes calldata) external payable returns (bytes memory) {
        uint256 callId = callNonce++;

        // Step 1: Send L1→L2 bridge message
        bytes memory payload = abi.encode(
            msg.sender, l2Target, msg.data, callId,
            address(this)
        );
        bytes memory bridgeData = abi.encodeWithSignature("onMessageInvocation(bytes)", payload);
        IBridge(bridge).sendMessage(IBridge.Message({
            id: 0, fee: 0, gasLimit: 2000000,
            from: address(0), srcChainId: 0,
            srcOwner: address(this), destChainId: destChainId,
            destOwner: address(this), to: l2Receiver,
            value: 0, data: bridgeData
        }));

        // Step 2: Execute propose (ZK proof verification)
        bool proposeOk;
        try ProofStoreV2(proofStore).executePropose(callId) returns (bool ok) {
            proposeOk = ok;
        } catch {}
        if (!proposeOk) revert ProofNotLoaded();

        // Step 3: Load builder-provided return data
        (uint64 returnMsgId, bool success, bytes memory retData, bytes memory proof, uint256 storedCallId) =
            ProofStoreV2(proofStore).getReturnProof(callId);

        if (returnMsgId == 0 && !success && retData.length == 0) revert ProofNotLoaded();
        require(storedCallId == callId, "callId mismatch");

        // Step 4: Reconstruct the expected return message from known fields + claimed data.
        // The return message was sent by L2Receiver via bridge.sendMessage on L2.
        // We know all fields except `id` (provided by builder as returnMsgId).
        // The return payload contains (callId, success, retData).
        // Security: the return message hash (verified by proveSignalReceived) includes:
        //   - 'to' field = address(this) (this proxy) in the Message struct
        //   - callId = this proxy's nonce in the payload
        //   - retData = the claimed return value in the payload
        // All three are part of bridge.hashMessage(). A malicious builder cannot forge
        // a valid return because it would need a real L2 execution that sent a message
        // to this exact proxy address with this exact callId — which only happens when
        // THIS proxy's outbound message is processed on L2.
        // the return to this specific call. The "to" field is set to this proxy
        // address, and callId is the proxy's nonce — both are in the hash.
        // 
        bytes memory returnMsgData = abi.encodeWithSignature(
            "onMessageInvocation(bytes)",
            abi.encode(callId, success, retData)
        );

        IBridge.Message memory returnMsg = IBridge.Message({
            id: returnMsgId,
            fee: 0,
            gasLimit: 1000000,
            from: l2Receiver,
            srcChainId: destChainId,
            srcOwner: l2Receiver,
            destChainId: srcChainId,
            destOwner: l2Receiver,
            to: address(this),
            value: 0,
            data: returnMsgData
        });

        // Compute the hash using the bridge's own hashMessage function.
        // This guarantees the hash matches what the L2 bridge computed.
        bytes32 expectedHash = IBridgeHasher(bridge).hashMessage(returnMsg);

        // Step 5: Verify the reconstructed hash exists in the ZK-proven L2 state.
        // If the builder lied about retData, returnMsgId, or success, the hash
        // won't match any signal in L2 state, and this call reverts.
        ISignalService(signalService).proveSignalReceived(
            destChainId, l2Bridge, expectedHash, proof
        );

        // Step 6: Return verified data. If L2 call failed, propagate the revert.
        if (!success) {
            assembly {
                revert(add(retData, 32), mload(retData))
            }
        }

        return retData;
    }

    receive() external payable {}
}
