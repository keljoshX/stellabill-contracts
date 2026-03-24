# Batch Operation Tests Documentation

This document describes the consistency-focused test suite for batch charge operations in the
Stellabill subscription vault contract.

## Overview

The batch charge functionality (`batch_charge`) allows the admin to process multiple subscription
charges in a single transaction. The function continues processing all subscriptions even if some
fail, returning a result for each subscription ID.

The primary guarantee covered by these tests is that `batch_charge` behaves exactly like applying
`charge_subscription` sequentially to the same input list at the same ledger timestamp.

## Test Coverage Summary

### Test Groups Added

1. **Single-vs-Batch Equivalence** - Successful batch charges match repeated single charges
2. **Mixed Outcome Consistency** - Valid, invalid, duplicate, and missing IDs preserve order and codes
3. **Failed Item Isolation** - Failed items produce no extra side effects beyond single-call semantics
4. **High-Volume Lists** - Larger input lists remain deterministic and ledger-correct

## Key Findings

- ✅ Batch output matches sequential single-call semantics for identical inputs
- ✅ Partial failures don't introduce extra side effects beyond the single path
- ✅ Error codes remain stable and ordered index-for-index with the input list
- ✅ Duplicate IDs behave like repeated single charges at the same timestamp
- ✅ Ledger state and merchant balances stay consistent for high-volume inputs

## Test Statistics

- **Error types covered:** `NotFound`, `InsufficientBalance`, `NotActive`, `Replay`
- **Batch sizes tested:** Small deterministic lists and high-volume lists with duplicates
- **Execution time:** Full test suite remains under a few seconds locally

## Behaviors Validated

### Batch-vs-Single Consistency
- `batch_charge` returns the same success/error pattern as repeated `charge_subscription` calls
- Result ordering matches the input list exactly
- Duplicate IDs observe prior successful items exactly as they would on sequential single calls

### Ledger Correctness
- Successful charges deduct prepaid balance, update timestamps, and credit merchant balances
- Failed charges may still apply the same documented single-call state transitions
  such as grace-period movement on insufficient balance
- Batch processing must not introduce any extra state mutations beyond those single-call effects

### Error Handling
- InsufficientBalance (1003): Not enough prepaid balance
- IntervalNotElapsed (1001): Billing period not reached
- NotActive (1002): Subscription paused or cancelled
- NotFound (404): Invalid subscription ID

### Edge Cases
- Mixed valid and invalid IDs
- Duplicate IDs in one batch
- Missing subscription IDs
- High-volume input lists with alternating success/failure states

## Usage Recommendations

1. **Treat results positionally:** Each result belongs to the input ID at the same index.
2. **Retry selectively:** Only retry failed entries after fixing their root cause.
3. **Expect duplicate sensitivity:** Duplicate IDs later in the same list may fail because earlier entries already mutated state.
4. **Audit failed items through single semantics:** If a failed batch item changes state, it should match what a direct single charge would have done.

## Conclusion

✅ Batch charge behavior is proven consistent with sequential single-charge semantics
✅ Ordering, error-code stability, and failed-item isolation are covered
✅ High-volume deterministic behavior is documented and tested
