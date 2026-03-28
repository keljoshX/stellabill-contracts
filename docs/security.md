# Security Threat Model: Subscription Vault Contract

## Overview

This document consolidates the overarching security assumptions, threat models, and corresponding mitigations for the Stellabill Subscription Vault smart contract on the Soroban network. It aggregates the deep-dives found in `docs/reentrancy.md`, `docs/replay_protection.md`, and `docs/safe_math.md` into a single, concise threat matrix mapping directly to contract functions and tests.

**Primary Asset**: USDC in prepaid subscription balances  
**Last Updated**: 2026-03-23

---

## 1. Threat Model Assumptions

### Trusted Actors
- **Admin**: Operates with High trust. Responsible for charging subscriptions and setting parameters (e.g., `min_topup`). Represents a single point of failure.
- **Soroban Runtime**: Trusted to securely execute WebAssembly, enforce authentication (`require_auth()`), and manage state natively.
- **USDC Token Contract**: Trusted to implement the Stellar Asset Contract (SAC) correctly without malicious arbitrary callbacks.

### Semi & Untrusted Actors
- **Subscribers (Semi-Trusted)**: Can deposit funds and manage their own subscriptions but might attempt state manipulations to avoid charges.
- **Merchants (Semi-Trusted)**: Can withdraw earned balances but might attempt to over-withdraw or exploit cross-subscription data.
- **External Attackers (Untrusted)**: May probe the contract for reentrancy, overflow, or state-bypass vulnerabilities.

---

## 2. Threat Categories & Mitigation Mapping

| Threat Category | Risk Description | Implemented Mitigations | Associated Functions | Test Coverage Verification |
|-----------------|------------------|-------------------------|----------------------|----------------------------|
| **Unauthorized Access** | Attackers or unauthorized users performing state-changing actions. | All state changes require Soroban `require_auth()` signature verification. Operational roles (Admin, Subscriber, Merchant) are explicitly validated. | `charge_subscription`, `batch_charge`, `deposit_funds`, `cancel_subscription`, `set_min_topup`, `rotate_admin` | `test_charge_subscription_unauthorized`, `test_cancel_subscription_unauthorized`, `test_set_min_topup_unauthorized`, `test_unauthorized_merchant_config_update` |
| **Reentrancy (Callbacks)** | Malicious token contracts or other external calls triggering recursive execution to drain funds or bypass state updates. | Strict adherence to the **Checks-Effects-Interactions (CEI)** pattern. Internal balances are updated in storage *before* any `token.transfer()` occurs. Optional runtime locks (`reentrancy.rs`) are available. | `do_deposit_funds`, `withdraw_merchant_funds`, `do_withdraw_subscriber_funds` | `test_deposit_funds_state_committed_before_transfer`, `test_withdraw_merchant_funds_state_committed_before_transfer`, `test_reentrancy_lock_prevents_recursive_calls` |
| **Replay & Double Charging** | Re-executing a charge transaction within the same period to debit a subscriber multiple times. | Period-based tracking (`now >= last_payment_timestamp + interval_seconds`) ensures single deduction per interval. Deduplication relies on `idempotency_key` handling for retries without double-debits (`Error::Replay`). | `charge_subscription`, `batch_charge` | `test_charge_succeeds_at_exact_interval`, `test_immediate_retry_at_same_timestamp_rejected`, `test_replay_protection_on_batch_charge` |
| **Arithmetic Risks** | Integer overflow or underflow leading to incorrect balances or state corruption. | Explicit use of Rust's checked arithmetic wrappers (`safe_add`, `safe_sub`) returning explicit `Error::Overflow` (500) and `Error::Underflow` (501). Prevents any silent wraparound or negative balances. | `safe_add_balance`, `safe_sub_balance`, `charge_one` | `test_safe_add_overflow_returns_error`, `test_safe_sub_underflow_returns_error`, `test_charge_amount_greater_than_balance_fails` |
| **State Machine Bypass** | Forcing a subscription into an invalid state (e.g., Active to Paused without proper authorization). | Centralized validation via `validate_status_transition`. Subscriptions locked once `Cancelled`. Illegal state jumping yields `Error::InvalidStatusTransition`. | `validate_status_transition`, `do_cancel_subscription`, `do_pause_subscription` | `test_all_valid_transitions_coverage`, `test_invalid_cancelled_to_active`, `test_validate_cancelled_transitions_all_blocked` |
| **ID Collision / Overflow** | Reaching the maximum subscription ID and wrapping around to overwrite existing data. | Monotonically increasing, atomic `next_id` counter bounded to `u32::MAX`. | `next_id`, `create_subscription` | `test_subscription_limit_reached` |

---

## 3. Security Regression Pack

The Security Regression Pack (found in `src/test_security.rs`) consolidates critical safety checks to ensure no regressions are introduced during future development. It is grouped into four primary risk classes:

### Risk Class 1: Reentrancy & Flow Control
- **Negative Test**: Verify that a recursive call to `withdraw_merchant_funds` is blocked by the `ReentrancyGuard`.
- **Consistency Test**: Ensure all state updates (e.g., balance decrements) occur *before* external token transfers.

### Risk Class 2: Authorization & Ownership
- **Negative Test**: Attempt to pause or cancel a subscription using a "stranger" address (neither subscriber nor merchant).
- **Negative Test**: Verify that `rotate_admin` and `recover_stranded_funds` fail when called by a non-admin.

### Risk Class 3: Replay & Idempotency
- **Negative Test**: Attempt to charge the same subscription twice within the same ledger timestamp.
- **Negative Test**: Verify that `batch_charge` deduplicates redundant IDs within a single call.

### Risk Class 4: Arithmetic Bounds
- **Edge Case Test**: Verify that charging a subscription with exactly `amount` leaves the balance at zero.
- **Negative Test**: Attempt to `deposit_funds` with a negative amount (underflow protection).
- **Negative Test**: Verify that `safe_add` correctly returns `Error::Overflow` at `i128::MAX`.

---

## 4. Known Limitations and Future Hardening

While the current architecture rigorously applies the CEI pattern and strict arithmetic bounds, the following systemic risks represent known limitations slated for future mitigation:

1. **Admin Key Compromise**: 
   - *Risk*: A compromised admin key can force global charges or manipulate thresholds continuously.
   - *Future Hardening*: Transition to multi-signature structures or time-locked upgrades for critical controls.
2. **Storage Exhaustion (DoS)**: 
   - *Risk*: An attacker with valid signatures can spam `create_subscription` indefinitely, inflating ledger footprint arbitrarily.
   - *Future Hardening*: Introduce minimal initial deposit requirements, per-subscriber creation limits, and archival functions.
3. **Owner Verification Gap**: 
   - *Risk*: Actions like `pause_subscription` accept an `authorizer` without rigorously asserting whether that authorizer strictly matches `sub.subscriber` or `sub.merchant`.
   - *Future Hardening*: Patch state-changing endpoints with explicit owner cross-checks: `if authorizer != sub.subscriber && ... { return Err; }`.
4. **No Initialization Lock (`init`)**:
   - *Risk*: The vault initialization endpoint can be accidentally re-called, potentially overwriting admin references.
   - *Future Hardening*: Wrap `init` logic with a check for presence of a stored initialization constant (`Error::AlreadyInitialized`).

---

## Document Maintenance

For deeper mechanics and mathematical constraints, review the underlying architecture documentation:
- **`docs/reentrancy.md`**: Logic isolation, execution order (CEI constraints)
- **`docs/replay_protection.md`**: Idempotency keys, clock skew resistance
- **`docs/safe_math.md`**: Fixed-point bounds checking and token translation
- **`docs/subscription_state_machine.md`**: Terminal transitions and automation hooks
