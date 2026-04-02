// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// @notice Calls a target with calldata, logs the result.
contract Logger {
    struct CallResult {
        bool success;
        bytes data;
    }

    mapping(uint256 => CallResult) public results;
    uint256 public callCount;

    event Called(uint256 indexed id, address indexed target, bytes calldata_, bool success, bytes result);

    function execute(address target, bytes calldata calldata_) external returns (bool success, bytes memory result) {
        (success, result) = target.call(calldata_);
        uint256 id = callCount++;
        results[id] = CallResult(success, result);
        emit Called(id, target, calldata_, success, result);
    }
}
