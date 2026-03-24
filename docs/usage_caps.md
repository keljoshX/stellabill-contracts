# Usage Caps

Subscriptions with `usage_enabled=true` can define an optional per-period hard cap and a lifetime cap.

## Per-Period Caps

Configured via `configure_usage_limits`:
- `usage_cap_units: Option<i128>`

### Behavior:
- Usage charges increment `current_period_usage_units` in `UsageState`.
- If a charge would cause `current_period_usage_units + amount > usage_cap_units`, the transaction is rejected with `UsageCapExceeded (1034)`.
- The period index is tracked as `now / interval_seconds`. When the contract rolls into a new billing period, `current_period_usage_units` automatically resets to `0` before applying the new charge.

### Notes:
- Caps are configured by the merchant for each subscription.
- The rejection path is deterministic and storage-efficient (no iteration over past statements required).

## Lifetime Caps

Configured at subscription creation (or inherited from plan templates):
- `lifetime_cap: Option<i128>`

### Behavior:
- Both interval charges and usage charges increment `lifetime_charged`.
- If a usage charge exceeds the remaining lifetime cap:
  - The charge is aborted.
  - The subscription is automatically transitioned to `Cancelled`.
  - A `LifetimeCapReachedEvent` is emitted.
