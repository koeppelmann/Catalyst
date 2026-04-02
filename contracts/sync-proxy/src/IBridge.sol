// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

interface IBridge {
    struct Message {
        uint64 id;
        uint64 fee;
        uint32 gasLimit;
        address from;
        uint64 srcChainId;
        address srcOwner;
        uint64 destChainId;
        address destOwner;
        address to;
        uint256 value;
        bytes data;
    }

    function sendMessage(Message calldata _message)
        external
        payable
        returns (bytes32 msgHash, Message memory message_);
}
