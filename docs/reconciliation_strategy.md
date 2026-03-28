# Billing Statement Reconciliation Strategy

When billing statements are pruned (compacted), the detailed history is replaced by a `BillingStatementAggregate`. To maintain financial reporting accuracy and perform reconciliation, follow this strategy.

## 1. Aggregate Structure
The `BillingStatementAggregate` stores the summary of all pruned statements:
- `pruned_count`: Total number of rows removed.
- `total_amount`: Sum of `amount` across all removed rows.
- `totals`: Per-kind breakdown (`interval`, `usage`, `one_off`).
- `oldest_period_start` / `newest_period_end`: Time range covered by pruned data.

## 2. Reconstructing Full History
To calculate the total billed amount for a subscription since its creation:
```
Total Billed = Aggregate.total_amount + Sum(LiveStatements.amount)
```

To calculate the breakdown per charge kind:
```
Total Interval = Aggregate.totals.interval + Sum(LiveStatements where kind == Interval)
Total Usage = Aggregate.totals.usage + Sum(LiveStatements where kind == Usage)
Total One-Off = Aggregate.totals.one_off + Sum(LiveStatements where kind == OneOff)
```

## 3. Verification & Integrity
- **Sequence Integrity**: The lowest `sequence` number in the live statements should be equal to `Aggregate.pruned_count`. Any gap indicates a data integrity issue.
- **Count Consistency**: `SubscriptionVault::get_total_statements` returns the count of *live* statements. The total number of statements ever created can be inferred as `Aggregate.pruned_count + LiveCount`.
- **Amount Consistency**: The `Subscription::lifetime_charged` field should always equal the sum of all billing statements (compacted + live).
    - Note: Differences may arise if refunds were processed, which are tracked separately in `MerchantEarnings`.

## 4. Reconciliation Workflow
1. Call `get_stmt_compacted_aggregate(subscription_id)` to get the summary of pruned history.
2. Call `get_sub_statements_offset` or `get_sub_statements_cursor` to fetch live detailed rows.
3. Sum the values as described above.
4. Compare against `get_subscription(subscription_id).lifetime_charged` for high-level validation.
