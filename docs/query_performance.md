# Query Performance Guardrails

This document outlines the performance characteristics and safety limits implemented for the `subscription_vault` contract to ensure predictable execution costs and prevent resource exhaustion in high-volume accounts.

## Read Complexity Reference

Storage reads are the primary driver of execution costs in Soroban. The following table identifies the read complexity for each query endpoint.

| Endpoint | Read Complexity | Guardrail | Notes |
|---|---|---|---|
| `get_subscription` | O(1) | None | Direct persistent storage lookup. |
| `get_subscriptions_by_merchant` | O(N) | `MAX_SUBSCRIPTION_LIST_PAGE` (100) | Reads full merchant index `Vec<u32>` + up to `limit` records. |
| `get_subscriptions_by_token` | O(N) | `MAX_SUBSCRIPTION_LIST_PAGE` (100) | Reads full token index `Vec<u32>` + up to `limit` records. |
| `list_subscriptions_by_subscriber` | O(MAX_SCAN_DEPTH) | `MAX_SCAN_DEPTH` (1,000) | Performs a linear ID scan. Caps at 1,000 IDs per call. |
| `get_cap_info` | O(1) | None | Single record read. |
| `estimate_topup` | O(1) | None | Single record read. |

## Safety Limits

To prevent "out of gas" errors or excessive fees during write operations, the following hard limits are enforced:

### 1. `MAX_SCAN_DEPTH` (1,000)
This limit applies to the **Subscriber Query Path**. Since there is no secondary index for subscribers, the contract must scan the global subscription sequence.
- **Behavior**: If the requested page is not filled after scanning 1,000 IDs, the call returns the current partial result and a `next_start_id` cursor.
- **Client Impact**: Clients should use the `next_start_id` to continue scanning if they receive an empty or incomplete list.

### 2. `MAX_WRITE_PATH_SCAN_DEPTH` (5,000)
This limit applies to **Write Path Checks** (e.g., Credit Limit enforcement, Plan Concurrency).
- **Behavior**: If a subscriber has no configuration that requires an O(n) scan (e.g., no credit limit set), the contract uses a "fast-path" skip. If a scan is required and the contract size exceeds 5,000 IDs, the operation returns `Error::InvalidInput`.
- **Rationale**: High-volume merchants (>5,000 total subscriptions under one vault) should avoid using per-subscriber write-path features to maintain performance.

### 3. `MAX_SUBSCRIPTION_LIST_PAGE` (100)
Applies to index-based pagination (`get_subscriptions_by_merchant`, `get_subscriptions_by_token`).
- **Behavior**: Requests for `limit > 100` are rejected to prevent excessive storage footprint in a single transaction.

## Best Practices for High-Volume Accounts

1. **Use Merchant Indices**: Querying by merchant is O(1) for the index and O(limit) for records. This is significantly more efficient than scanning by subscriber.
2. **Pre-fetch Counts**: Use `get_merchant_subscription_count` to determine if an account needs heavy pagination before starting.
3. **Avoid write-path scans**: For merchants expecting >5,000 subscriptions, skip using per-subscriber credit limits or plan-concurrency caps to ensure `create_subscription` remains fast.
