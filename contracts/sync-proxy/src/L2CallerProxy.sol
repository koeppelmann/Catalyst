// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// @notice Represents an L1 caller on L2. Only the L2Receiver can invoke it.
contract L2CallerProxy {
    address public immutable l1Caller;
    address public immutable receiver;

    constructor(address _l1Caller, address _receiver) {
        l1Caller = _l1Caller;
        receiver = _receiver;
    }

    function forward(address target, bytes calldata data)
        external
        returns (bool success, bytes memory ret)
    {
        require(msg.sender == receiver, "only receiver");
        (success, ret) = target.call(data);
    }
}
