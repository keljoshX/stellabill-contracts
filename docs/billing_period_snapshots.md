# Billing Period Snapshots

Each subscription now stores a compact `BillingPeriodSnapshot` per closed period, keyed by:

- subscription id
- period index (starting at 0 from subscription creation)

Each snapshot records:

- period start and end timestamps
- total amount charged during that period
- total usage units charged during that period
- status flags (closed, interval charged, usage charged, empty)

Snapshots are written only when a successful charge closes one or more elapsed periods.
Failed charges do not create snapshots.

Retention strategy:

- snapshots are immutable once written
- old snapshots can be pruned or compacted by off-chain indexers after export
- period index ordering preserves historical continuity even if old records are archived

## Integrity Verification

Snapshots include built-in integrity checks:

- Period boundaries: period_start <= period_end
- Amount validation: amount > 0
- Interval charges require period_start < period_end
- Sequencing: monotonic sequence numbers across all charge kinds
- Compaction aggregates match pruned statement sums

These invariants ensure data consistency for reporting pipelines and prevent corruption from invalid inputs.

## Usage for Reporting Pipelines

Snapshots serve as the primary data source for billing reports:

- Each snapshot represents a complete billing period
- Compacted aggregates provide summary data for pruned periods
- Status flags indicate charge types processed in each period
- Timestamps enable temporal analysis and period alignment
- Immutable nature ensures audit trail integrity
