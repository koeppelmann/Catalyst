// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// @notice Simple addition contract on L2. Tracks a number per caller.
contract Addition {
    mapping(address => uint256) public numbers;

    function add(uint256 value) external returns (uint256) {
        numbers[msg.sender] += value;
        return numbers[msg.sender];
    }

    function read() external view returns (uint256) {
        return numbers[msg.sender];
    }
}
