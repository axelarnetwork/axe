use alloy::sol;

sol! {
    #[sol(rpc)]
    contract Governance {
        function gateway() external view returns (address);
        function governanceChainHash() external view returns (bytes32);
        function governanceAddressHash() external view returns (bytes32);
    }

    #[sol(rpc)]
    contract Managed {
        function owner() external view returns (address);
        function operator() external view returns (address);
        function isOperator(address account) external view returns (bool);
        function gateway() external view returns (address);
        function transferOwnership(address newOwner) external;
    }
}
