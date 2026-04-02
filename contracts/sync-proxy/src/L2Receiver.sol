// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IBridge} from "./IBridge.sol";
import {L2CallerProxy} from "./L2CallerProxy.sol";
import {L2CallerProxyFactory} from "./L2CallerProxyFactory.sol";

interface IBridgeContext {
    function context() external view returns (bytes32 msgHash, address from, uint64 srcChainId);
}

/// @notice Singleton on L2. Receives L1->L2 bridge messages, routes through caller proxies,
///         and bridges return values back to L1.
contract L2Receiver {
    address public immutable bridge;
    L2CallerProxyFactory public immutable proxyFactory;
    uint64 public immutable l1ChainId;

    event CrossChainCallExecuted(
        uint256 indexed callId,
        address indexed l1Caller,
        address indexed l2Target,
        bool success,
        bytes returnData
    );
    event ReturnBridgeFailed(uint256 indexed callId, bytes reason);

    constructor(address _bridge, address _proxyFactory, uint64 _l1ChainId) {
        bridge = _bridge;
        proxyFactory = L2CallerProxyFactory(_proxyFactory);
        l1ChainId = _l1ChainId;
    }

    function onMessageInvocation(bytes calldata _data) external {
        require(msg.sender == bridge, "only bridge");
        (bytes32 outboundMsgHash, address srcApp,) = IBridgeContext(bridge).context();

        (
            address l1Caller,
            address l2Target,
            bytes memory callData,
            uint256 callId,
            address resultStoreL1
        ) = abi.decode(_data, (address, address, bytes, uint256, address));

        // Ensure the return is only bridged back to the originating L1 app.
        require(srcApp == resultStoreL1, "src/result mismatch");

        address callerProxy = _getOrDeployProxy(l1Caller);
        (bool success, bytes memory ret) = L2CallerProxy(callerProxy).forward(l2Target, callData);

        emit CrossChainCallExecuted(callId, l1Caller, l2Target, success, ret);

        if (resultStoreL1 != address(0)) {
            _bridgeReturn(callId, success, ret, resultStoreL1, outboundMsgHash);
        }
    }

    function _bridgeReturn(
        uint256 callId,
        bool success,
        bytes memory ret,
        address resultStoreL1,
        bytes32 outboundMsgHash
    ) internal {
        bytes memory resultPayload = abi.encode(callId, success, ret, outboundMsgHash);
        bytes memory bridgeData = abi.encodeWithSignature(
            "onMessageInvocation(bytes)",
            resultPayload
        );

        try IBridge(bridge).sendMessage(IBridge.Message({
            id: 0,
            fee: 0,
            gasLimit: 1000000,
            from: address(0),
            srcChainId: 0,
            srcOwner: address(this),
            destChainId: l1ChainId,
            destOwner: address(this),
            to: resultStoreL1,
            value: 0,
            data: bridgeData
        })) {} catch (bytes memory reason) {
            emit ReturnBridgeFailed(callId, reason);
        }
    }

    function _getOrDeployProxy(address l1Caller) internal returns (address) {
        address predicted = proxyFactory.getAddress(l1Caller, address(this));
        if (predicted.code.length > 0) return predicted;
        return proxyFactory.deploy(l1Caller, address(this));
    }
}
