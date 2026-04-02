// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {L2CallerProxy} from "./L2CallerProxy.sol";

/// @notice Deploys deterministic L2CallerProxy instances via CREATE2.
contract L2CallerProxyFactory {
    event ProxyDeployed(address indexed l1Caller, address proxy);

    function deploy(address l1Caller, address receiver) external returns (address proxy) {
        bytes32 salt = keccak256(abi.encodePacked(l1Caller));
        proxy = address(new L2CallerProxy{salt: salt}(l1Caller, receiver));
        emit ProxyDeployed(l1Caller, proxy);
    }

    function getAddress(address l1Caller, address receiver) external view returns (address) {
        bytes32 salt = keccak256(abi.encodePacked(l1Caller));
        bytes32 hash = keccak256(abi.encodePacked(
            bytes1(0xff),
            address(this),
            salt,
            keccak256(abi.encodePacked(
                type(L2CallerProxy).creationCode,
                abi.encode(l1Caller, receiver)
            ))
        ));
        return address(uint160(uint256(hash)));
    }
}
