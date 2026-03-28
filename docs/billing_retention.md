# Billing statement retention and compaction

This contract supports bounded billing-statement growth using configurable retention and explicit compaction.

## Retention model

- Detailed rows are append-only per subscription, keyed by sequence.
- `set_billing_retention(admin, keep_recent)` configures the default number of detailed rows to keep.
- `get_billing_retention()` returns current policy.

Default policy keeps all rows until retention is explicitly set.

## Compaction flow

- `compact_billing_statements(admin, subscription_id, keep_recent_override)`
- Admin-only operation
- Prunes oldest detailed rows beyond keep threshold
- Preserves high-level auditability in `BillingStatementAggregate`:
  - `pruned_count`
  - `total_amount`
  - `oldest_period_start`
  - `newest_period_end`

Read aggregate with `get_stmt_compacted_aggregate(subscription_id)`.

## Guarantees and limits

- Compaction never mutates remaining detailed rows.
- Remaining rows preserve sequence IDs and ordering.
- Pagination APIs continue to return stable ordering over active rows.
- Pruned detail is intentionally irreversible; only aggregate totals remain.

## Guidance

- Choose `keep_recent` to match frontend history window (for example, 12-24 periods).
- Run compaction periodically for high-volume subscriptions.

## Security and operations

- Only the contract admin may call `set_billing_retention` and `compact_billing_statements` (see `require_admin_auth` in the vault). Holders of other roles cannot change retention or prune history.
- `keep_recent == 0` is allowed: a compaction run can remove **all** detailed rows for a subscription while cumulative pruned amounts and period bounds remain in `BillingStatementAggregate` (via `get_stmt_compacted_aggregate`).
- Each successful compaction emits `billing_compacted` with a summary that includes run totals (`pruned_count`, `kept_count`, `total_pruned_amount`) and the post-run aggregate (`aggregate_*` fields) so indexers can reconcile against `get_stmt_compacted_aggregate` without extra reads.
- Repeated compaction with the same effective threshold is a no-op (zero rows pruned) once the live row count is at or below the keep threshold—safe to run on a schedule.
- When tuning retention in production, prefer staged changes (set default policy, then compact high-traffic subscriptions with `keep_recent_override` before lowering the global default) so operators can validate exports and dashboards.
