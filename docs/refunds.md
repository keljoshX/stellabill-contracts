## Partial refunds for mid-period downgrades and cancellations

The subscription vault supports controlled partial refunds so that merchants and operators can
return a portion of a subscriber's prepaid balance when plans are downgraded or cancelled
mid-period, without compromising balance integrity.

### Design goals

- **Safety first** – No fund creation or loss; all refunds are debits from existing balances.
- **Explicit authorization** – Only the contract admin can authorize partial refunds.
- **Predictable semantics** – Refunds operate on remaining prepaid balances and do not
  retroactively alter past charges.
- **Clear observability** – Each refund emits a dedicated event for off-chain reconciliation.

### Entry point

`partial_refund(admin, subscription_id, subscriber, amount) -> Result<(), Error>`

#### Authorization model

- `admin` must be the stored contract admin. The call is gated via `require_admin_auth`,
  which calls `admin.require_auth()` and verifies the address matches the stored admin.
- `subscriber` is a **validation target only** — it is checked against the subscription's
  `subscriber` field to prevent misdirected refunds, but the subscriber does **not** need
  to co-sign the transaction. The admin acts on the subscriber's behalf.

This design allows a backend operations service running as the contract admin to issue
refunds without requiring the subscriber to be online or to sign.

#### Preconditions

| Condition | Error on failure |
|-----------|-----------------|
| `admin` is the stored contract admin | `Unauthorized` |
| `amount > 0` | `InvalidAmount` |
| `subscriber` matches `subscription.subscriber` | `Unauthorized` |
| `amount <= subscription.prepaid_balance` | `InsufficientBalance` |

#### Effects (CEI pattern)

1. **Checks** — all preconditions validated before any state change.
2. **Effects** — `subscription.prepaid_balance` decremented by `amount` and persisted.
3. **Interactions** — token transfer from vault to subscriber executed after state update.

This ordering prevents reentrancy: if the token transfer re-enters the contract, the
balance has already been debited so a second refund of the same amount will fail the
`InsufficientBalance` check.

#### Event

Every successful partial refund emits:

```
topic:   ("partial_refund", subscription_id)
payload: PartialRefundEvent {
    subscription_id: u32,
    subscriber:      Address,
    amount:          i128,
    timestamp:       u64,
}
```

### Refund semantics

Partial refunds work against the **remaining prepaid balance**:

- Funds that have not yet been charged (unused balance) can be partially refunded.
- Previously processed charges that already credited merchant balances are not
  modified by this API; they remain part of the settlement history.
- Multiple successive partial refunds are allowed as long as each individual
  `amount <= current prepaid_balance` at the time of the call.
- A refund equal to the full remaining balance is valid ("full-balance-as-partial").
- Partial refunds are permitted on subscriptions in **any status**, including
  `Cancelled`. This supports the common pattern of cancelling first, then issuing
  a prorated refund of the remaining balance before the subscriber withdraws.

### Common flows

#### Cancellation with prorated refund

```
1. cancel_subscription(subscription_id, subscriber)
2. partial_refund(admin, subscription_id, subscriber, prorated_amount)
3. withdraw_subscriber_funds(subscription_id, subscriber)   // withdraw remainder
```

#### Mid-period downgrade

```
1. partial_refund(admin, subscription_id, subscriber, agreed_amount)
   // Future charges will use the new (lower) plan amount
```

### Security notes

- **Over-refund protection**: `amount > prepaid_balance` is rejected with
  `InsufficientBalance`. The contract cannot create tokens; it can only transfer
  what it holds.
- **Subscriber ownership check**: passing a `subscriber` address that does not match
  the subscription record returns `Unauthorized`, preventing refunds to wrong addresses.
- **Admin-only gate**: non-admin callers receive `Unauthorized` regardless of other
  parameters.
- **CEI ordering**: state is written before the token transfer, eliminating the
  reentrancy window present in naive implementations.

### Test coverage

| Scenario | Test |
|----------|------|
| Basic debit + token transfer | `test_partial_refund_debits_prepaid_and_transfers_tokens` |
| Zero amount rejected | `test_partial_refund_rejects_invalid_amounts_and_auth` |
| Negative amount rejected | `test_partial_refund_rejects_invalid_amounts_and_auth` |
| Over-refund rejected | `test_partial_refund_rejects_invalid_amounts_and_auth` |
| Non-admin rejected | `test_partial_refund_rejects_invalid_amounts_and_auth` |
| Wrong subscriber rejected | `test_partial_refund_rejects_invalid_amounts_and_auth` |
| Repeated refunds are cumulative | `test_partial_refund_repeated_debits_are_cumulative` |
| Cumulative drain then over-refund fails | `test_partial_refund_cumulative_exact_drain_then_over_refund_fails` |
| Full balance as partial | `test_partial_refund_full_balance_as_partial_succeeds` |
| Refund after cancellation | `test_partial_refund_after_cancellation_succeeds` |
| Event emission | `test_partial_refund_emits_event` |
