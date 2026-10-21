// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

contract Counter {
    uint256 public value;

    event ValueChanged(address indexed caller, uint256 value);

    function set(uint256 next) external {
        value = next;
        emit ValueChanged(msg.sender, next);
    }

    function fail() external pure {
        revert("demo revert");
    }
}
