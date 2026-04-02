// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SyncL1ProxyV2} from "./SyncL1ProxyV2.sol";

/// @notice Deploys deterministic SyncL1ProxyV2 instances via CREATE2.
///         One proxy per L2 target contract.
contract SyncL1ProxyV2Factory {
    address public immutable bridge;
    address public immutable l2Receiver;
    address public immutable l2Bridge;
    address public immutable signalService;
    address public immutable proofStore;
    uint64 public immutable destChainId;
    uint64 public immutable srcChainId;

    event ProxyDeployed(address indexed l2Target, address proxy);

    constructor(
        address _bridge,
        address _l2Receiver,
        address _l2Bridge,
        address _signalService,
        address _proofStore,
        uint64 _destChainId,
        uint64 _srcChainId
    ) {
        bridge = _bridge;
        l2Receiver = _l2Receiver;
        l2Bridge = _l2Bridge;
        signalService = _signalService;
        proofStore = _proofStore;
        destChainId = _destChainId;
        srcChainId = _srcChainId;
    }

    function deploy(address l2Target) external returns (address proxy) {
        bytes32 salt = keccak256(abi.encodePacked(l2Target));
        proxy = address(new SyncL1ProxyV2{salt: salt}(
            l2Target, bridge, l2Receiver, l2Bridge, signalService, proofStore, destChainId, srcChainId
        ));
        emit ProxyDeployed(l2Target, proxy);
    }

    function getAddress(address l2Target) external view returns (address) {
        bytes32 salt = keccak256(abi.encodePacked(l2Target));
        bytes32 hash = keccak256(abi.encodePacked(
            bytes1(0xff),
            address(this),
            salt,
            keccak256(abi.encodePacked(
                type(SyncL1ProxyV2).creationCode,
                abi.encode(l2Target, bridge, l2Receiver, l2Bridge, signalService, proofStore, destChainId, srcChainId)
            ))
        ));
        return address(uint160(uint256(hash)));
    }
}
