# Test contracts

Example L2 target contracts used to verify the sync proxy system end-to-end.

## Contracts

| Contract | Purpose |
|----------|---------|
| `CrossChainCounter.sol` | Per-caller increment counter. Tracks `counts[msg.sender]` and emits `Incremented` events. |
| `Addition.sol` | Parameterized addition. `add(uint256 value)` adds to the caller's number and returns the new total. `read()` returns the current number. |
| `Logger.sol` | L1 helper that calls a target with arbitrary calldata and logs the result (emits `Called` event, stores result in mapping). Useful for observing proxy return values on L1. |

## End-to-end test flow

```
Safe → Logger.execute(SyncProxy(Addition), add(100))
  → Logger calls SyncProxy with add(100)
    → SyncProxy forwards to L2 Addition via bridge
    → L2 Addition.add(100) executes, returns 142
    → SyncProxy returns 142 to Logger
  → Logger stores {success: true, data: 0x...8e}
  → Logger emits Called(id, proxy, add(100), true, 142)
```

Verified on Gnosis chain (L1) + Surge testnet (L2) with real ZK proofs.
