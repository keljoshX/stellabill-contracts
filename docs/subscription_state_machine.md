# Subscription State Machine Documentation

For full lifecycle, on-chain representation, and invariants see [subscription_lifecycle.md](subscription_lifecycle.md).

## Overview

This document describes the state machine for `SubscriptionStatus` in the SubscriptionVault contract. The state machine enforces valid lifecycle transitions to prevent invalid states and ensure data integrity.

## States

The subscription can be in one of four states:

| State | Description | Entry Conditions |
|-------|-------------|------------------|
| **Active** | Subscription is active and charges can be processed | Default state after creation, or resumed from Paused/InsufficientBalance |
| **Paused** | Subscription is temporarily suspended, no charges are processed | Paused from Active state by subscriber or merchant |
| **Cancelled** | Subscription is permanently terminated | Cancelled from Active, Paused, or InsufficientBalance |
| **InsufficientBalance** | Subscription cannot be charged until explicitly resumed | Can follow `GracePeriod` expiration, or be set by self-managed interface in test setup |
| **GracePeriod** | Temporary window after an insufficient-balance charge during which a top-up and charge may recover to Active | Entered when `charge_subscription` on `Active` has insufficient funds and grace period is configured |

## State Diagram

```
                    ┌─────────────────────────────────────────┐
                    │                                         │
                    ▼                                         │
┌─────────┐    ┌─────────┐    ┌─────────┐    ┌─────────────────┴─┐
│  START  │───▶│  ACTIVE │───▶│ PAUSED  │───▶│   CANCELLED       │
└─────────┘    └────┬────┘    └────┬────┘    │   (Terminal)      │
                    │              │         └───────────────────┘
                    │              │                    ▲
                    │              └────────────────────┤
                    │                                   │
                    │         ┌──────────────────────┐  │
                    └────────▶│ INSUFFICIENT_BALANCE │──┘
                              └──────────────────────┘
```

## Grace-period semantics

Grace period is managed by charge flow on `Active` subscriptions.

- `Active` → `GracePeriod`: on `charge_subscription()` failure due to insufficient prepaid balance, only when grace period config > 0.
- `GracePeriod` entry timestamp is recorded as `due_timestamp = last_payment_timestamp + interval_seconds`.
- `GracePeriod` remains allowed for all pause/cancel paths accepted by state machine.
- `GracePeriod` → `InsufficientBalance`: when current ledger `now` is at or past `grace_start_timestamp + grace_period` and charge still fails.
- `GracePeriod` → `Active`: on successful deferred charge while still in grace window.

`charge_one` and `batch_charge` are aligned so the status updates in `GracePeriod` are persisted inside failed-charge path, and entrypoint returns semantic success (for transition).  `Error::InsufficientBalance` is still returned at call boundary for the initial insufficient-charge event.

### Valid Transitions

| From | To | Method | Description |
|------|-----|--------|-------------|
| Active | Paused | `pause_subscription()` | Temporarily pause billing |
| Active | Cancelled | `cancel_subscription()` | Permanently cancel subscription |
| Active | InsufficientBalance | `charge_subscription()` (auto) | Charge failed due to insufficient balance |
| Paused | Active | `resume_subscription()` | Resume billing |
| Paused | Cancelled | `cancel_subscription()` | Cancel while paused |
| InsufficientBalance | Active | `resume_subscription()` | Resume after deposit |
| InsufficientBalance | Cancelled | `cancel_subscription()` | Cancel due to funding issues |
| *any* | Same | (idempotent) | Setting same status is always allowed |

### Invalid Transitions (Blocked)

| From | To | Why Blocked |
|------|-----|-------------|
| Cancelled | Active | Terminal state - no reactivation |
| Cancelled | Paused | Terminal state - no changes allowed |
| Cancelled | InsufficientBalance | Terminal state - no changes allowed |
| Paused | InsufficientBalance | Cannot fail charge on paused subscription |
| InsufficientBalance | Paused | Must either fund and resume, or cancel |

## Implementation

### Core Helper Functions

The state machine is implemented through helper functions in `contracts/subscription_vault/src/lib.rs`:

```rust
/// Validates if a status transition is allowed
pub fn validate_status_transition(
    from: &SubscriptionStatus,
    to: &SubscriptionStatus,
) -> Result<(), Error>

/// Returns valid target statuses for a given state
pub fn get_allowed_transitions(status: &SubscriptionStatus) -> &'static [SubscriptionStatus]

/// Boolean check for transition validity
pub fn can_transition(from: &SubscriptionStatus, to: &SubscriptionStatus) -> bool
```

### Error Handling

Invalid transitions return `Error::InvalidStatusTransition` (error code 400) without mutating storage. Lifecycle-related errors from `contracts/subscription_vault/src/types.rs`:

```rust
pub enum Error {
    NotFound = 404,
    Unauthorized = 401,
    IntervalNotElapsed = 1001,
    NotActive = 1002,
    InvalidStatusTransition = 400,
    BelowMinimumTopup = 402,
    Overflow = 403,
    Underflow = 1004,
    InsufficientBalance = 1003,
}
```

### Usage in Entrypoints

All state-changing entrypoints use `validate_status_transition` before updating status:

```rust
pub fn cancel_subscription(...) -> Result<(), Error> {
    authorizer.require_auth();
    let mut sub = Self::get_subscription(env.clone(), subscription_id)?;
    
    // Enforce state machine
    validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
    sub.status = SubscriptionStatus::Cancelled;
    
    env.storage().instance().set(&subscription_id, &sub);
    Ok(())
}
```

## Property Test Model

The test suite includes deterministic property-style checks that validate the state machine under
arbitrary action sequences without adding extra dependencies.

Model assumptions:

- The transition rules are defined independently in the tests as a manual adjacency model.
- Property tests compare the manual model against:
  - `validate_status_transition`
  - `can_transition`
  - `get_allowed_transitions`
  - lifecycle entrypoints `pause_subscription`, `resume_subscription`, and `cancel_subscription`
- Charge failure paths are also sampled to verify:
  - helper-level rules only allow `Active -> GracePeriod` and `Active -> InsufficientBalance`
  - failed public `charge_subscription` calls do not persist an illegal stored status change
  - recovery flows remain constrained to allowed lifecycle actions

These tests are pseudo-random but reproducible because they use a fixed deterministic generator.

## Examples

### Example 1: Normal Lifecycle

```rust
// Create subscription (starts as Active)
let id = client.create_subscription(&subscriber, &merchant, &amount, &interval, &false);
// Status: Active

// Pause the subscription
client.pause_subscription(&id, &subscriber);
// Status: Paused (Active -> Paused: Valid)

// Resume later
client.resume_subscription(&id, &subscriber);
// Status: Active (Paused -> Active: Valid)

// Eventually cancel
client.cancel_subscription(&id, &subscriber);
// Status: Cancelled (Active -> Cancelled: Valid)
```

### Example 2: Insufficient Balance Flow

```rust
// Subscription is Active
let id = client.create_subscription(&subscriber, &merchant, &amount, &interval, &false);

// Charge fails due to insufficient balance
// (Internally: Active -> InsufficientBalance)

// User deposits more funds and resumes
client.resume_subscription(&id, &subscriber);
// Status: Active (InsufficientBalance -> Active: Valid)
```

### Example 3: Blocked Transition (Error)

```rust
// Cancelled subscription
let id = client.create_subscription(&subscriber, &merchant, &amount, &interval, &false);
client.cancel_subscription(&id, &subscriber);
// Status: Cancelled

// This will fail with InvalidStatusTransition
try {
    client.resume_subscription(&id, &subscriber);  // ERROR!
} catch (Error::InvalidStatusTransition) {
    // Cannot resume cancelled subscription
}
```

## Test Coverage

The state machine has comprehensive test coverage in `contracts/subscription_vault/src/test.rs`:

- **Valid transitions**: 7 valid transitions tested
- **Invalid transitions**: 6+ invalid transition attempts tested
- **Idempotent transitions**: Same-state transitions tested
- **Full lifecycle sequences**: Multi-step transition flows tested
- **Entrypoint integration**: All entrypoints enforce state machine

## Extending the State Machine

To add a new status:

1. Add the new variant to `SubscriptionStatus` enum
2. Update `validate_status_transition` with allowed transitions
3. Update `get_allowed_transitions` to include new status
4. Add entrypoint methods for transitions involving the new status
5. Add tests for all new transitions (valid and invalid)
6. Update this documentation

## Migration Notes

If existing data has subscriptions in unexpected states:

1. Query all subscriptions and their current statuses
2. For any subscription in an unexpected state, determine appropriate remediation
3. Consider adding a one-time admin migration function for edge cases
4. After migration, all subscriptions will follow the enforced state machine

## Security Considerations

- **Storage integrity**: Invalid transitions return errors before any storage mutation
- **Authorization**: Each transition still requires proper authorization (subscriber/merchant)
- **Terminal state**: Cancelled is irreversible by design - prevents accidental reactivation
- **Predictability**: Clear rules make behavior predictable and auditable
