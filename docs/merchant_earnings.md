# Merchant earnings accounting

`SubscriptionVault` tracks merchant earnings as an internal per-merchant ledger that is independent from individual subscription records. It supports highly granular tracking for deterministic reconciliation.

## Model

- Each successful charge (`charge_subscription`, `charge_usage`, `charge_one_off`) debits a subscription's `prepaid_balance` by its `amount`.
- The same amount is credited to `merchant_balance[subscription.merchant]`.
- Merchant balances are tracked by token.
- A highly granular `TokenEarnings` struct records accruals, withdrawals, and refunds for reconciliation.

### Data Structures

- **TokenEarnings**: Tracks `accruals` (broken down by Interval, Usage, OneOff), `withdrawals`, and `refunds`.
- **ReconciliationSnapshot**: Contains `total_accruals`, `total_withdrawals`, `total_refunds`, and `computed_balance` for external verification.

## Withdrawal behavior

- `withdraw_merchant_funds(merchant, amount)` requires merchant auth.
- It validates `amount > 0` and `merchant_balance >= amount`.
- On success it debits internal merchant balance, then transfers tokens from vault custody to the merchant wallet.
- The withdrawn amount is added to `TokenEarnings.withdrawals`.

## Refund behavior

- `merchant_refund(merchant, subscriber, token, amount)` requires merchant auth.
- It debits internal merchant balance and transfers tokens from vault custody to the subscriber.
- The refunded amount is added to `TokenEarnings.refunds`.

## Invariants

1. For each successful charge, `subscription.prepaid_balance` decreases by exactly `amount`.
2. For each successful charge, `merchant_balance[merchant, token]` increases by exactly `amount`.
3. For each successful merchant withdrawal, `merchant_balance[merchant, token]` decreases by exactly withdrawn amount.
4. For each successful merchant refund, `merchant_balance[merchant, token]` decreases by exactly refunded amount.
5. Merchant balances are isolated by merchant address and token, and must not leak across merchants.
6. Contract state updates and token transfer happen in one transaction; if token transfer fails, the transaction aborts and state is reverted.
7. **Reconciliation Invariant**: `reported_balance = total_accruals - total_withdrawals - total_refunds`. This must always match `merchant_balance`.

## Reporting & Indexers

Backend indexers can use the following APIs to retrieve and verify merchant earnings:

- `get_merchant_total_earnings(merchant)`: Returns detailed earnings breakdown for all tokens the merchant has interacted with.
- `get_merchant_token_earnings(merchant, token)`: Returns detailed earnings for a specific token.
- `get_reconciliation_snapshot(merchant)`: Returns computed snapshots that verify the reconciliation invariant (`computed_balance`).

To reconstruct balances off-chain, indexers should listen to:
- `charged` events (Interval charges)
- `charge_usage` events (not explicitly a separate event, handled generically or via statements)
- `oneoff_ch` events
- `withdrawn` events
- `merchant_refund` events

## Security notes

- Charge logic rejects non-`Active` subscriptions and returns `InsufficientBalance` for underfunded subscriptions.
- Internal accounting uses checked arithmetic (`checked_add`, `checked_sub`) to prevent silent overflow/underflow.
- Earnings are accrued internally before payout; funds remain in contract custody until explicit merchant withdrawal.
