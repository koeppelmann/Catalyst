// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// @notice Test target on L2. Tracks per-caller increment counts.
contract CrossChainCounter {
    mapping(address => uint256) public counts;
    address[] public callerLog;

    event Incremented(address indexed caller, uint256 newCount);

    function increment() external returns (uint256) {
        counts[msg.sender]++;
        callerLog.push(msg.sender);
        emit Incremented(msg.sender, counts[msg.sender]);
        return counts[msg.sender];
    }

    function getCallerLog() external view returns (address[] memory) {
        return callerLog;
    }
}
