# Synchronous Cross-Chain Proxy

A generic proxy system that enables L1 contracts to call L2 contracts and receive return values **synchronously within a single L1 transaction**.

Any call to an L1 proxy is forwarded to the corresponding L2 contract via the Taiko bridge. The builder constructs the L2 block, obtains a ZK proof, and restructures the L1 multicall so that `propose()` and `proveSignalReceived()` execute **inline** during the proxy call. The L2 return value is verified trustlessly and returned to the caller as if the L2 contract lived on L1.

## How it works

```
multicall([
  Call 0: ProofStoreV2.store(propose_calldata, return_proof)   ← builder pre-loads
  Call 1: Safe → SyncProxy.anyFunction(args)
            ├─ bridge.sendMessage(L1→L2)
            ├─ ProofStoreV2.executePropose()    → ZK proof verified
            ├─ SignalService.proveSignalReceived() → return value verified
            └─ return L2_result                 ← synchronous!
])
```

## Contracts

### L1 (Gnosis)

| Contract | Purpose |
|----------|---------|
| `SyncL1ProxyV2` | Proxy that forwards any call to L2 and returns the result synchronously. Reverts with `ProofNotLoaded()` if called without the builder. Reverts with the L2 revert reason if the L2 call fails. |
| `SyncL1ProxyV2Factory` | CREATE2 factory — deploys a deterministic proxy for any L2 target address. |
| `ProofStoreV2` | Transient storage (EIP-1153) for pre-loaded proof data. Auto-clears after the transaction. Stores propose calldata + return message hash + hop proof. |
| `IBridge` | Shared Taiko bridge interface. |

### L2 (Surge)

| Contract | Purpose |
|----------|---------|
| `L2Receiver` | Singleton that receives bridge messages and routes calls through per-caller proxies. Bridges return values back to L1. |
| `L2CallerProxy` | Represents an L1 address on L2. Preserves `msg.sender` identity — the L2 target sees a deterministic address unique to each L1 caller. |
| `L2CallerProxyFactory` | CREATE2 factory for L2 caller proxies. Deployed lazily on first call from a new L1 address. |

## Caller identity

Each L1 caller gets a deterministic proxy on L2:

```
L1: Safe (0xd926...)  →  L2: L2CallerProxy(Safe) at 0x6f54...
L1: Logger (0xDEE3...) →  L2: L2CallerProxy(Logger) at 0x6747...
```

The L2 target contract sees `msg.sender = L2CallerProxy(L1_caller)`. Different L1 callers get different L2 proxies, so per-caller state (like `mapping(address => uint256)`) works correctly.

## Behavior

| Scenario | Result |
|----------|--------|
| Call with builder (proof loaded) | Returns L2 result synchronously |
| Call without builder (no proof) | Reverts with `ProofNotLoaded()` |
| L2 call reverts | Reverts with the same revert reason (transparent) |
| Wrong function selector on L2 | Reverts with L2's revert reason |

## Builder changes

The modified Catalyst builder (see `realtime/src/l1/proposal_tx_builder.rs`) supports two env vars:

- `SYNC_PROOF_STORE_ADDRESS` — address of the deployed ProofStoreV2. Enables sync multicall mode.
- `SYNC_MODE_V2` — if set, uses direct SignalService verification instead of Bridge.processMessage for the return path (~105K gas savings).

The builder also extracts bridge messages from call outputs (not just event logs) to support proxies that revert during simulation.

## Deployed addresses (Gnosis + Surge testnet)

**L1 (Gnosis, chain 100):**
- SyncL1ProxyV2Factory: `0x2281AF83f0887131506C78Ba9ccfa75BE3903f56`
- ProofStoreV2: `0xD28C7Cc7B071a4290EEe8a71Ebd18E49a7aAeF8f`

**L2 (Surge, chain 763374):**
- L2Receiver: `0x169CE8f007d91D63350eFF409876fdc692eEE60C`
- L2CallerProxyFactory: `0x0F5aD0C80a851b9EfE2c63E05aDDD8aEb52131DA`

## Gas cost

~1.16M gas per synchronous cross-chain call on Gnosis, including:
- ProofStoreV2.store(): ~191K (transient storage)
- bridge.sendMessage(): ~69K
- propose() + ZK verify: ~411K
- proveSignalReceived(): ~290K (Merkle proof)
- Safe + multicall overhead: ~80K
