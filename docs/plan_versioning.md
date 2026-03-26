## Plan template versioning and subscription migration

Plan templates describe reusable subscription offers (amount, interval, usage flag, and optional
lifetime cap). Merchants often need to evolve these offers over time while preserving backwards
compatibility for existing subscribers. This document describes how template versioning works in
the subscription vault and how to safely migrate subscriptions between versions.

### Data model

Each `PlanTemplate` now carries two additional fields:

- `template_key: u32` – logical template group identifier shared by all versions of the same
  plan. The first version of a template uses its own plan ID as the `template_key`.
- `version: u32` – monotonically increasing version number within a template group, starting
  at `1` for the initial definition.

Plan templates are immutable once created. Updating a template always creates a new plan
record with the same `template_key` and an incremented `version`, so old versions remain
addressable for existing subscriptions and for audit purposes.

Subscriptions created from a plan template are linked to the specific plan ID they were
instantiated from. This linkage is kept in contract storage, allowing the contract to reason
about the template family and version during migrations.

### Updating plan templates

Merchants update templates via the `update_plan_template` entrypoint:

- Creates a **new** `PlanTemplate` record.
- Preserves the existing `template_key`.
- Increments `version` by 1 relative to the previous version.
- Keeps the settlement token unchanged (token changes must use a new template family).

Existing subscriptions are **not** modified by this call. New subscribers that use the
new plan ID automatically receive the updated terms, while older subscribers continue to
run on their original plan version until explicitly migrated.

An event `plan_template_updated` is emitted for each update, carrying:

- `template_key`
- `old_plan_id` and `new_plan_id`
- `version` of the new plan
- `merchant` and `timestamp`

### Migrating subscriptions between versions

Subscribers can opt into newer template versions via the
`migrate_subscription_to_plan` entrypoint:

- Caller must be the subscription `subscriber` (subscriber auth required).
- The subscription must have been created from a plan template; direct, ad‑hoc
  subscriptions cannot be migrated this way.
- Migration is only allowed when:
  - The old and new plans share the same `template_key`.
  - The target plan has a strictly higher `version` than the current one.
  - The settlement token does not change.
  - Any configured lifetime cap on the new plan is **not** already exceeded by the
    subscription’s current `lifetime_charged` value.

On successful migration:

- `amount`, `interval_seconds`, and `usage_enabled` of the subscription are updated to
  match the new plan.
- `lifetime_cap` is replaced with the new plan’s cap (or cleared when the new plan is
  uncapped), while `lifetime_charged` is preserved.
- The internal linkage from subscription → plan ID is updated to point at the new plan.

A `subscription_migrated` event is emitted with:

- `subscription_id`
- `template_key`
- `from_plan_id` and `to_plan_id`
- `merchant`, `subscriber`, and `timestamp`

### Compatibility and upgrade guidelines

- **No silent changes** – Existing subscriptions never change behavior when a plan is
  updated. Only new subscribers and explicitly migrated subscriptions use the new version.
- **Token stability** – Plan updates cannot switch the underlying settlement token; such
  changes must be modeled as a new template family (new `template_key`) with clear UX.
- **Lifetime caps** – When moving to a more restrictive cap, the contract enforces that
  the cap is not already exceeded. If exceeded, migration fails and the caller must
  choose a compatible plan or keep the existing one.
- **Frontend UX** – Frontends should:
  - Fetch plan metadata (including `template_key` and `version`) to display upgrade
    options.
  - Show explicit confirmation flows describing changes in price, interval, or usage
    policies before calling `migrate_subscription_to_plan`.
  - Use events and read‑only queries to reconcile which subscribers are on which
    template version.

## Security Considerations

**Key Safety Properties (enforced by `migrate_subscription_to_plan`):**

| Property | Protection | Error Code |
|----------|------------|------------|
| Monotonic version progression | `new.version > current.version` | `InvalidInput` (#1015) |
| Same template lineage | `template_key` match | `InvalidInput` (#1015) |
| No token mismatches | Token address equality | `InvalidInput` (#1015) |
| Compatible lifetime caps | `lifetime_charged ≤ new_cap` | `LifetimeCapReached` (#1017) |
| No billing model changes | `usage_enabled` unchanged | `InvalidInput` (#1015) |
| Subscriber opt-in only | Caller == `sub.subscriber` | `Forbidden` (#403) |

**Attack Vectors Mitigated:**
- **Downgrade attacks**: Cannot migrate to older `version`.
- **Cross-plan confusion**: `template_key` pins lineage.
- **Token drain**: Token pinned, cannot switch to attacker-controlled token.
- **Cap bypass**: New cap cannot be smaller than existing `lifetime_charged`.
- **Unauthorized migration**: Only subscription owner can migrate.
- **Reentrancy**: Pure storage reads before any mutation; CEI-compliant.

**Test Coverage:**
Comprehensive unit tests cover all failure modes including same-version, downgrade, cap-exceeded, token-mismatch, non-plan subs.

**Audit Notes:**
- Gas bounded O(1), no loops.
- No external calls in migration path.
- Events logged for full auditability.
- Backward compatible: existing subs unchanged.


