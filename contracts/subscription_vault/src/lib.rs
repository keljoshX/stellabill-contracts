#![no_std]
#![allow(clippy::too_many_arguments)]

//! Prepaid subscription vault for recurring USDC billing.
//!
//! For subscription lifecycle, status transitions, and on-chain representation
//! see `docs/subscription_lifecycle.md`.
//!
//! For lifetime charge cap semantics see `docs/lifetime_caps.md`.
//!
//! For metadata key-value store see `docs/subscription_metadata.md`.

// ── Modules ──────────────────────────────────────────────────────────────────
mod admin;
mod blocklist;
mod charge_core;
mod merchant;
mod metadata;
pub mod migration;
mod oracle;
mod queries;
mod reentrancy;
pub mod safe_math;
mod state_machine;
mod statements;
mod subscription;
#[cfg(test)]
mod test;
mod types;
#[cfg(test)]
mod test_refactor_check;
#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod test_governance;
#[cfg(test)]
mod test_insufficient_balance;
#[cfg(test)]
mod test_security;


#[cfg(test)]
mod test_safe_math_regression;
#[cfg(test)]
mod test_usage_limits;
#[cfg(test)]
mod test_deterministic_charging;

use soroban_sdk::{contract, contractimpl, Address, Env, String, Symbol, Vec};

// ── Re-exports ────────────────────────────────────────────────────────────────
pub use blocklist::{BlocklistAddedEvent, BlocklistEntry, BlocklistRemovedEvent};
pub use queries::compute_next_charge_info;
pub use state_machine::{can_transition, get_allowed_transitions, validate_status_transition};
pub use types::{
    AcceptedToken, AdminRotatedEvent, BatchChargeResult, BatchWithdrawResult, BillingChargeKind,
    BillingCompactedEvent, BillingCompactionSummary, BillingRetentionConfig, BillingStatement,
    BillingStatementAggregate, BillingStatementsPage, CapInfo, ChargeExecutionResult, ContractSnapshot, DataKey,
    EmergencyStopDisabledEvent, EmergencyStopEnabledEvent, Error, FundsDepositedEvent,
    LifetimeCapReachedEvent, MerchantPausedEvent, MerchantUnpausedEvent, MerchantWithdrawalEvent,
    MetadataDeletedEvent, MetadataSetEvent, MigrationExportEvent, NextChargeInfo,
    OneOffChargedEvent, OracleConfig, OraclePrice, PartialRefundEvent, PlanTemplate,
    PlanTemplateUpdatedEvent, RecoveryEvent, RecoveryReason, Subscription,
    SubscriptionCancelledEvent, SubscriptionChargedEvent, SubscriptionChargeFailedEvent, SubscriptionCreatedEvent,
    SubscriptionMigratedEvent, SubscriptionPausedEvent, SubscriptionResumedEvent,
    SubscriptionStatus, SubscriptionSummary, UsageLimits, UsageState, UsageStatementEvent,
    AccruedTotals, MerchantConfig, TokenEarnings, TokenReconciliationSnapshot,
    MAX_METADATA_KEYS, MAX_METADATA_KEY_LENGTH, MAX_METADATA_VALUE_LENGTH,
};
/// Maximum subscription ID this contract will ever allocate.
///
/// When the counter reaches this value [`SubscriptionVault::create_subscription`]
/// returns [`Error::SubscriptionLimitReached`] instead of wrapping or panicking.
pub const MAX_SUBSCRIPTION_ID: u32 = u32::MAX;

const STORAGE_VERSION: u32 = 2;
const MAX_EXPORT_LIMIT: u32 = 100;

// ── Internal helpers ──────────────────────────────────────────────────────────

fn require_admin_auth(env: &Env, admin: &Address) -> Result<(), Error> {
    admin.require_auth();
    let stored_admin = admin::require_admin(env)?;
    if admin != &stored_admin {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn get_emergency_stop(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "emergency_stop"))
        .unwrap_or(false)
}

fn require_not_emergency_stop(env: &Env) -> Result<(), Error> {
    if get_emergency_stop(env) {
        return Err(Error::EmergencyStopActive);
    }
    Ok(())
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct SubscriptionVault;

#[contractimpl]
impl SubscriptionVault {
    // ── Admin / Config ────────────────────────────────────────────────────────

    /// Initialize the contract: set token address, admin, minimum top-up, and grace period.
    pub fn init(
        env: Env,
        token: Address,
        token_decimals: u32,
        admin: Address,
        min_topup: i128,
        grace_period: u64,
    ) -> Result<(), Error> {
        admin::do_init(&env, token, token_decimals, admin, min_topup, grace_period)
    }

    /// Update the minimum top-up threshold. Only callable by admin.
    pub fn set_min_topup(env: Env, admin: Address, min_topup: i128) -> Result<(), Error> {
        admin::do_set_min_topup(&env, admin, min_topup)
    }

    /// Get the current minimum top-up threshold.
    pub fn get_min_topup(env: Env) -> Result<i128, Error> {
        admin::get_min_topup(&env)
    }

    /// Get the current admin address.
    pub fn get_admin(env: Env) -> Result<Address, Error> {
        admin::do_get_admin(&env)
    }

    /// Rotate admin to a new address. Only callable by current admin.
    pub fn rotate_admin(env: Env, current_admin: Address, new_admin: Address) -> Result<(), Error> {
        admin::do_rotate_admin(&env, current_admin, new_admin)
    }

    /// Recover stranded funds from the contract. Admin only.
    pub fn recover_stranded_funds(
        env: Env,
        admin: Address,
        recipient: Address,
        amount: i128,
        reason: RecoveryReason,
    ) -> Result<(), Error> {
        admin::do_recover_stranded_funds(&env, admin, recipient, amount, reason)
    }

    /// Charge a batch of subscriptions in one transaction. Admin only.
    ///
    /// **Disabled when emergency stop is active.**
    ///
    /// Returns a per-subscription result vector so callers can identify
    /// which charges succeeded and which failed (with error codes).
    pub fn batch_charge(
        env: Env,
        subscription_ids: Vec<u32>,
    ) -> Result<Vec<BatchChargeResult>, Error> {
        require_not_emergency_stop(&env)?;
        admin::do_batch_charge(&env, &subscription_ids)
    }

    // ── Emergency Stop ────────────────────────────────────────────────────────

    /// Get the current emergency stop status.
    pub fn get_emergency_stop_status(env: Env) -> bool {
        get_emergency_stop(&env)
    }

    /// Enable the emergency stop (circuit breaker). Admin only.
    pub fn enable_emergency_stop(env: Env, admin: Address) -> Result<(), Error> {
        require_admin_auth(&env, &admin)?;
        if get_emergency_stop(&env) {
            return Ok(());
        }
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "emergency_stop"), &true);
        env.events().publish(
            (Symbol::new(&env, "emergency_stop_enabled"),),
            EmergencyStopEnabledEvent {
                admin,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(())
    }

    /// Disable the emergency stop (circuit breaker). Admin only.
    ///
    /// When disabled, normal contract operations resume. This should only be used
    /// after the incident has been resolved and the contract is safe to operate.
    pub fn disable_emergency_stop(env: Env, admin: Address) -> Result<(), Error> {
        require_admin_auth(&env, &admin)?;
        if !get_emergency_stop(&env) {
            return Ok(());
        }
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "emergency_stop"), &false);
        env.events().publish(
            (Symbol::new(&env, "emergency_stop_disabled"),),
            EmergencyStopDisabledEvent {
                admin,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(())
    }

    // ── Migration / Export ────────────────────────────────────────────────────

    /// **ADMIN ONLY**: Export contract-level configuration for migration tooling.
    pub fn export_contract_snapshot(env: Env, admin: Address) -> Result<ContractSnapshot, Error> {
        require_admin_auth(&env, &admin)?;

        let token: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token"))
            .ok_or(Error::NotFound)?;
        let min_topup: i128 = admin::get_min_topup(&env)?;
        let next_id: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "next_id"))
            .unwrap_or(0);

        env.events().publish(
            (Symbol::new(&env, "migration_contract_snapshot"),),
            (admin.clone(), env.ledger().timestamp()),
        );

        Ok(ContractSnapshot {
            admin,
            token,
            min_topup,
            next_id,
            storage_version: STORAGE_VERSION,
            timestamp: env.ledger().timestamp(),
        })
    }

    /// Export a single subscription summary for migration tooling. Admin only.
    pub fn export_subscription_summary(
        env: Env,
        admin: Address,
        subscription_id: u32,
    ) -> Result<SubscriptionSummary, Error> {
        require_admin_auth(&env, &admin)?;
        let sub = queries::get_subscription(&env, subscription_id)?;

        env.events().publish(
            (Symbol::new(&env, "migration_export"),),
            MigrationExportEvent {
                admin: admin.clone(),
                start_id: subscription_id,
                limit: 1,
                exported: 1,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(SubscriptionSummary {
            subscription_id,
            subscriber: sub.subscriber,
            merchant: sub.merchant,
            token: sub.token,
            amount: sub.amount,
            interval_seconds: sub.interval_seconds,
            last_payment_timestamp: sub.last_payment_timestamp,
            status: sub.status,
            prepaid_balance: sub.prepaid_balance,
            usage_enabled: sub.usage_enabled,
            lifetime_cap: sub.lifetime_cap,
            lifetime_charged: sub.lifetime_charged,
        })
    }

    /// Export a paginated list of subscription summaries. Admin only.
    pub fn export_subscription_summaries(
        env: Env,
        admin: Address,
        start_id: u32,
        limit: u32,
    ) -> Result<Vec<SubscriptionSummary>, Error> {
        require_admin_auth(&env, &admin)?;
        if limit > MAX_EXPORT_LIMIT {
            return Err(Error::InvalidExportLimit);
        }
        if limit == 0 {
            return Ok(Vec::new(&env));
        }

        let next_id: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "next_id"))
            .unwrap_or(0);
        if start_id >= next_id {
            return Ok(Vec::new(&env));
        }

        let end_id = start_id.saturating_add(limit).min(next_id);
        let mut out = Vec::new(&env);
        let mut exported = 0u32;
        let mut id = start_id;
        while id < end_id {
            if let Some(sub) = env.storage().instance().get::<u32, Subscription>(&id) {
                out.push_back(SubscriptionSummary {
                    subscription_id: id,
                    subscriber: sub.subscriber,
                    merchant: sub.merchant,
                    token: sub.token,
                    amount: sub.amount,
                    interval_seconds: sub.interval_seconds,
                    last_payment_timestamp: sub.last_payment_timestamp,
                    status: sub.status,
                    prepaid_balance: sub.prepaid_balance,
                    usage_enabled: sub.usage_enabled,
                    lifetime_cap: sub.lifetime_cap,
                    lifetime_charged: sub.lifetime_charged,
                });
                exported += 1;
            }
            id += 1;
        }

        env.events().publish(
            (Symbol::new(&env, "migration_export"),),
            MigrationExportEvent {
                admin,
                start_id,
                limit,
                exported,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(out)
    }

    // ── Subscription Lifecycle ────────────────────────────────────────────────

    /// Create a new subscription.
    ///
    /// **Disabled when emergency stop is active.**
    ///
    /// # Arguments
    ///
    /// * `lifetime_cap` - Optional maximum total amount (token base units) that may ever be
    ///   charged for this subscription. `None` means no cap. When the cumulative charged
    ///   amount reaches this value, the subscription is cancelled automatically.
    ///   See `docs/lifetime_caps.md` for full semantics.
    ///
    /// # Errors
    /// Returns [`Error::SubscriptionLimitReached`] if the contract has already allocated
    /// [`MAX_SUBSCRIPTION_ID`] subscriptions and can issue no more unique IDs.
    pub fn create_subscription(
        env: Env,
        subscriber: Address,
        merchant: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_create_subscription(
            &env,
            subscriber,
            merchant,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Create a subscription pinned to a specific accepted token.
    #[allow(clippy::too_many_arguments)]
    pub fn create_subscription_with_token(
        env: Env,
        subscriber: Address,
        merchant: Address,
        token: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_create_subscription_with_token(
            &env,
            subscriber,
            merchant,
            token,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Subscriber deposits USDC into their prepaid vault.
    ///
    /// **Disabled when emergency stop is active.**
    pub fn deposit_funds(
        env: Env,
        subscription_id: u32,
        subscriber: Address,
        amount: i128,
    ) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_deposit_funds(&env, subscription_id, subscriber, amount)
    }

    /// Creates a plan template that can be used to instantiate subscriptions.
    ///
    /// # Arguments
    ///
    /// * `lifetime_cap` - Optional default lifetime cap applied to subscriptions
    ///   created from this template. `None` means template subscriptions have no cap.
    pub fn create_plan_template(
        env: Env,
        merchant: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        subscription::do_create_plan_template(
            &env,
            merchant,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Creates a token-specific plan template.
    pub fn create_plan_template_with_token(
        env: Env,
        merchant: Address,
        token: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        subscription::do_create_plan_template_with_token(
            &env,
            merchant,
            token,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Creates a subscription from a predefined plan template.
    pub fn create_subscription_from_plan(
        env: Env,
        subscriber: Address,
        plan_template_id: u32,
    ) -> Result<u32, Error> {
        subscription::do_create_subscription_from_plan(&env, subscriber, plan_template_id)
    }

    /// Retrieves a plan template by its ID.
    pub fn get_plan_template(env: Env, plan_template_id: u32) -> Result<PlanTemplate, Error> {
        subscription::get_plan_template(&env, plan_template_id)
    }

    /// Updates an existing plan template by creating a new version.
    ///
    /// This function never mutates the existing template in-place. Instead, it
    /// creates a new `PlanTemplate` sharing the same `template_key` with a
    /// monotonically increasing `version`. Existing subscriptions continue to
    /// use their original template until explicitly migrated.
    pub fn update_plan_template(
        env: Env,
        merchant: Address,
        plan_template_id: u32,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        subscription::do_update_plan_template(
            &env,
            merchant,
            plan_template_id,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Configure the maximum number of concurrent active subscriptions a subscriber
    /// may hold for a given plan template.
    ///
    /// When `max_active` is zero, no limit is enforced for that plan. This limit
    /// is checked when creating subscriptions from the plan via
    /// `create_subscription_from_plan`. Only the plan's merchant may call this.
    pub fn set_plan_max_active_subs(
        env: Env,
        merchant: Address,
        plan_template_id: u32,
        max_active: u32,
    ) -> Result<(), Error> {
        subscription::do_set_plan_max_active_subs(&env, merchant, plan_template_id, max_active)
    }

    /// Returns the configured max-active-subscriptions limit for a plan template.
    ///
    /// A value of `0` means no limit is enforced. This is the default when
    /// `set_plan_max_active_subs` has never been called for the given plan.
    pub fn get_plan_max_active_subs(env: Env, plan_template_id: u32) -> u32 {
        queries::get_plan_max_active_subs(&env, plan_template_id)
    }

    /// Migrates an existing subscription to a newer version of the same plan template.
    ///
    /// The subscriber must authorize this call. Migration is only allowed between
    /// plan versions that share the same `template_key`, and only from an older
    /// version to a newer one. The settlement token cannot change as part of
    /// migration, and lifetime caps are validated for compatibility.
    pub fn migrate_subscription_to_plan(
        env: Env,
        subscriber: Address,
        subscription_id: u32,
        new_plan_template_id: u32,
    ) -> Result<(), Error> {
        subscription::do_migrate_subscription_to_plan(
            &env,
            subscriber,
            subscription_id,
            new_plan_template_id,
        )
    }

    /// Set a per-subscriber credit limit for a specific settlement token.
    ///
    /// The limit is expressed in token base units and applies across all of the
    /// subscriber's subscriptions using that token. When the aggregate exposure
    /// (prepaid balances plus expected interval liabilities) would exceed this
    /// value, new subscriptions and top-ups are rejected.
    pub fn set_subscriber_credit_limit(
        env: Env,
        admin: Address,
        subscriber: Address,
        token: Address,
        limit: i128,
    ) -> Result<(), Error> {
        subscription::do_set_subscriber_credit_limit(&env, admin, subscriber, token, limit)
    }

    /// Read the configured credit limit for a subscriber and token.
    ///
    /// Returns 0 when no limit is configured, meaning "no limit".
    pub fn get_subscriber_credit_limit(env: Env, subscriber: Address, token: Address) -> i128 {
        subscription::get_subscriber_credit_limit(&env, subscriber, token)
    }

    /// Return the current aggregate exposure for a subscriber and token.
    ///
    /// Exposure is defined as the sum of prepaid balances plus the next-interval
    /// amounts for active subscriptions.
    pub fn get_subscriber_exposure(
        env: Env,
        subscriber: Address,
        token: Address,
    ) -> Result<i128, Error> {
        subscription::get_subscriber_exposure(&env, subscriber, token)
    }

    /// Cancel the subscription. Allowed from Active, Paused, or InsufficientBalance.
    /// Transitions to the terminal `Cancelled` state.
    pub fn cancel_subscription(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
    ) -> Result<(), Error> {
        subscription::do_cancel_subscription(&env, subscription_id, authorizer)
    }

    /// Subscriber withdraws their remaining prepaid balance after cancellation.
    pub fn withdraw_subscriber_funds(
        env: Env,
        subscription_id: u32,
        subscriber: Address,
    ) -> Result<(), Error> {
        subscription::do_withdraw_subscriber_funds(&env, subscription_id, subscriber)
    }

    /// Process a partial refund against a subscription's remaining prepaid balance.
    ///
    /// Only the contract admin may authorize partial refunds. The refunded amount
    /// is debited from the subscription's `prepaid_balance` and transferred back
    /// to the subscriber, following the same CEI pattern as other token flows.
    pub fn partial_refund(
        env: Env,
        admin: Address,
        subscription_id: u32,
        subscriber: Address,
        amount: i128,
    ) -> Result<(), Error> {
        subscription::do_partial_refund(&env, admin, subscription_id, subscriber, amount)
    }

    /// Pause subscription (no charges until resumed). Allowed from Active.
    pub fn pause_subscription(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
    ) -> Result<(), Error> {
        subscription::do_pause_subscription(&env, subscription_id, authorizer)
    }

    /// Resume a subscription to Active. Allowed from Paused, GracePeriod, or InsufficientBalance.
    pub fn resume_subscription(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
    ) -> Result<(), Error> {
        subscription::do_resume_subscription(&env, subscription_id, authorizer)
    }

    /// Merchant-initiated one-off charge against the subscription's prepaid balance.
    ///
    /// **This function is disabled when the emergency stop is active.**
    pub fn charge_one_off(
        env: Env,
        subscription_id: u32,
        merchant: Address,
        amount: i128,
    ) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_charge_one_off(&env, subscription_id, merchant, amount)
    }

    // ── Charging ──────────────────────────────────────────────────────────────

    /// Charge a subscription for one billing interval.
    ///
    /// **This function is disabled when the emergency stop is active.**
    ///
    /// Enforces strict interval timing and replay protection. Underfunded attempts
    /// move the subscription into a recoverable non-active state and emit a
    /// charge-failed event without mutating financial accounting fields.
    pub fn charge_subscription(env: Env, subscription_id: u32) -> Result<ChargeExecutionResult, Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_one(&env, subscription_id, env.ledger().timestamp(), None)
    }

    /// Charge a metered usage amount against the subscription's prepaid balance.
    ///
    /// **This function is disabled when the emergency stop is active.**
    pub fn charge_usage(env: Env, subscription_id: u32, usage_amount: i128) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_usage_one(
            &env,
            subscription_id,
            usage_amount,
            String::from_str(&env, "usage"),
        )
    }

    /// Charge a metered usage amount against the subscription's prepaid balance with a reference.
    ///
    /// **This function is disabled when the emergency stop is active.**
    pub fn charge_usage_with_reference(
        env: Env,
        subscription_id: u32,
        usage_amount: i128,
        reference: String,
    ) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_usage_one(&env, subscription_id, usage_amount, reference)
    }

    /// Configure usage rate limits and caps for a subscription. Merchant only.
    pub fn configure_usage_limits(
        env: Env,
        merchant: Address,
        subscription_id: u32,
        rate_limit_max_calls: Option<u32>,
        rate_window_secs: u64,
        burst_min_interval_secs: u64,
        usage_cap_units: Option<i128>,
    ) -> Result<(), Error> {
        subscription::do_configure_usage_limits(
            &env,
            merchant,
            subscription_id,
            rate_limit_max_calls,
            rate_window_secs,
            burst_min_interval_secs,
            usage_cap_units,
        )
    }

    // ── Merchant ──────────────────────────────────────────────────────────────

    /// Merchant withdraws accumulated USDC to their wallet.
    pub fn withdraw_merchant_funds(env: Env, merchant: Address, amount: i128) -> Result<(), Error> {
        merchant::withdraw_merchant_funds(&env, merchant, amount)
    }

    /// Merchant withdraw for a specific token bucket.
    pub fn withdraw_merchant_token_funds(
        env: Env,
        merchant: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), Error> {
        merchant::withdraw_merchant_funds_for_token(&env, merchant, token, amount)
    }

    /// Get the merchant's accumulated (uncharged) balance.
    pub fn get_merchant_balance(env: Env, merchant: Address) -> i128 {
        merchant::get_merchant_balance(&env, &merchant)
    }

    /// Token-scoped merchant balance.
    pub fn get_merchant_balance_by_token(env: Env, merchant: Address, token: Address) -> i128 {
        merchant::get_merchant_balance_by_token(&env, &merchant, &token)
    }

    /// Check if a merchant has enabled a blanket pause.
    pub fn get_merchant_paused(env: Env, merchant: Address) -> bool {
        merchant::get_merchant_paused(&env, merchant)
    }

    /// Enable a blanket pause for all of the merchant's subscriptions.
    pub fn pause_merchant(env: Env, merchant: Address) -> Result<(), Error> {
        merchant::pause_merchant(&env, merchant)
    }

    /// Disable a blanket pause for the merchant's subscriptions.
    pub fn unpause_merchant(env: Env, merchant: Address) -> Result<(), Error> {
        merchant::unpause_merchant(&env, merchant)
    }

    /// Process a refund from merchant balance back to a subscriber.
    pub fn merchant_refund(
        env: Env,
        merchant: Address,
        subscriber: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), Error> {
        merchant::merchant_refund(&env, merchant, subscriber, token, amount)
    }

    /// Get a reconciliation snapshot for all tokens used by a merchant.
    pub fn get_reconciliation_snapshot(
        env: Env,
        merchant: Address,
    ) -> Vec<crate::types::TokenReconciliationSnapshot> {
        merchant::get_reconciliation_snapshot(&env, &merchant)
    }

    /// Get total accrued earnings per token for a merchant.
    pub fn get_merchant_total_earnings(
        env: Env,
        merchant: Address,
    ) -> Vec<(Address, crate::types::TokenEarnings)> {
        merchant::get_merchant_total_earnings(&env, &merchant)
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    /// Read subscription by id.
    pub fn get_subscription(env: Env, subscription_id: u32) -> Result<Subscription, Error> {
        queries::get_subscription(&env, subscription_id)
    }

    /// Estimate how much a subscriber needs to deposit to cover N future intervals.
    pub fn estimate_topup_for_intervals(
        env: Env,
        subscription_id: u32,
        num_intervals: u32,
    ) -> Result<i128, Error> {
        queries::estimate_topup_for_intervals(&env, subscription_id, num_intervals)
    }

    /// Get estimated next charge info (timestamp + whether charge is expected).
    pub fn get_next_charge_info(env: Env, subscription_id: u32) -> Result<NextChargeInfo, Error> {
        let sub = queries::get_subscription(&env, subscription_id)?;
        Ok(compute_next_charge_info(&sub))
    }

    /// Return subscriptions for a merchant, paginated.
    pub fn get_subscriptions_by_merchant(
        env: Env,
        merchant: Address,
        start: u32,
        limit: u32,
    ) -> Vec<Subscription> {
        queries::get_subscriptions_by_merchant(&env, merchant, start, limit)
    }

    /// Return the total number of subscriptions ever created.
    pub fn get_subscription_count(env: Env) -> u32 {
        let key = Symbol::new(&env, "next_id");
        env.storage().instance().get(&key).unwrap_or(0u32)
    }

    /// Return the total number of subscriptions for a merchant.
    pub fn get_merchant_subscription_count(env: Env, merchant: Address) -> u32 {
        queries::get_merchant_subscription_count(&env, merchant)
    }

    /// List all subscription IDs for a given subscriber with pagination.
    pub fn list_subscriptions_by_subscriber(
        env: Env,
        subscriber: Address,
        start_from_id: u32,
        limit: u32,
    ) -> Result<crate::queries::SubscriptionsPage, Error> {
        crate::queries::list_subscriptions_by_subscriber(&env, subscriber, start_from_id, limit)
    }

    /// Get lifetime cap information for a subscription.
    ///
    /// Returns a [`CapInfo`] summary suitable for off-chain dashboards and UX displays.
    /// When no cap is configured all cap-related fields return `None` / `false`.
    pub fn get_cap_info(env: Env, subscription_id: u32) -> Result<CapInfo, Error> {
        queries::get_cap_info(&env, subscription_id)
    }

    /// Return subscription billing statements using offset/limit pagination.
    ///
    /// When `newest_first` is true (recommended for infinite scroll), offset 0
    /// starts from the most recent statement.
    pub fn get_sub_statements_offset(
        env: Env,
        subscription_id: u32,
        offset: u32,
        limit: u32,
        newest_first: bool,
    ) -> Result<BillingStatementsPage, Error> {
        statements::get_statements_by_subscription_offset(
            &env,
            subscription_id,
            offset,
            limit,
            newest_first,
        )
    }

    /// Return subscription billing statements using cursor pagination.
    ///
    /// - `cursor`: sequence index to start from (inclusive); pass `None` for first page.
    /// - `limit`: maximum number of statements to return.
    /// - `newest_first`: return recent history first when true.
    pub fn get_sub_statements_cursor(
        env: Env,
        subscription_id: u32,
        cursor: Option<u32>,
        limit: u32,
        newest_first: bool,
    ) -> Result<BillingStatementsPage, Error> {
        statements::get_statements_by_subscription_cursor(
            &env,
            subscription_id,
            cursor,
            limit,
            newest_first,
        )
    }

    /// Add a token to the accepted token registry. Admin only.
    pub fn add_accepted_token(
        env: Env,
        admin: Address,
        token: Address,
        decimals: u32,
    ) -> Result<(), Error> {
        admin::add_accepted_token(&env, admin, token, decimals)
    }

    /// Remove a non-default token from accepted token registry. Admin only.
    pub fn remove_accepted_token(env: Env, admin: Address, token: Address) -> Result<(), Error> {
        admin::remove_accepted_token(&env, admin, token)
    }

    /// List accepted token metadata.
    pub fn list_accepted_tokens(env: Env) -> Vec<AcceptedToken> {
        admin::list_accepted_tokens(&env)
    }

    /// Return subscriptions for a token, paginated by offset.
    pub fn get_subscriptions_by_token(
        env: Env,
        token: Address,
        start: u32,
        limit: u32,
    ) -> Vec<Subscription> {
        let key = (Symbol::new(&env, "token_subs"), token);
        let ids: Vec<u32> = env.storage().instance().get(&key).unwrap_or(Vec::new(&env));
        if limit == 0 || start >= ids.len() {
            return Vec::new(&env);
        }
        let end = if start + limit > ids.len() {
            ids.len()
        } else {
            start + limit
        };
        let mut out = Vec::new(&env);
        let mut i = start;
        while i < end {
            let id = ids.get(i).unwrap();
            if let Some(sub) = env.storage().instance().get::<u32, Subscription>(&id) {
                out.push_back(sub);
            }
            i += 1;
        }
        out
    }

    /// Configure statement retention (`keep_recent` detailed rows per subscription). Admin only.
    pub fn set_billing_retention(env: Env, admin: Address, keep_recent: u32) -> Result<(), Error> {
        require_admin_auth(&env, &admin)?;
        statements::set_retention_config(&env, keep_recent);
        Ok(())
    }

    /// Read current statement retention config.
    pub fn get_billing_retention(env: Env) -> BillingRetentionConfig {
        statements::get_retention_config(&env)
    }

    /// Return compacted aggregate totals for a subscription.
    pub fn get_stmt_compacted_aggregate(
        env: Env,
        subscription_id: u32,
    ) -> BillingStatementAggregate {
        statements::get_compacted_aggregate(&env, subscription_id)
    }

    /// Run compaction for one subscription. Admin only.
    pub fn compact_billing_statements(
        env: Env,
        admin: Address,
        subscription_id: u32,
        keep_recent_override: Option<u32>,
    ) -> Result<BillingCompactionSummary, Error> {
        require_admin_auth(&env, &admin)?;
        let summary = statements::compact_subscription_statements(
            &env,
            subscription_id,
            keep_recent_override,
        )?;
        env.events().publish(
            (Symbol::new(&env, "billing_compacted"), subscription_id),
            BillingCompactedEvent {
                admin,
                subscription_id,
                pruned_count: summary.pruned_count,
                kept_count: summary.kept_count,
                total_pruned_amount: summary.total_pruned_amount,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(summary)
    }

    /// Configure optional price oracle for cross-currency pricing. Admin only.
    pub fn set_oracle_config(
        env: Env,
        admin: Address,
        enabled: bool,
        oracle: Option<Address>,
        max_age_seconds: u64,
    ) -> Result<(), Error> {
        require_admin_auth(&env, &admin)?;
        oracle::set_oracle_config(&env, enabled, oracle, max_age_seconds)
    }

    /// Read the currently configured oracle integration settings.
    pub fn get_oracle_config(env: Env) -> OracleConfig {
        oracle::get_oracle_config(&env)
    }

    // ── Metadata ──────────────────────────────────────────────────────────────

    /// Set or update a metadata key-value pair on a subscription.
    ///
    /// Authorization: subscriber or merchant.
    /// Does not affect financial state (balances, status, charges).
    pub fn set_metadata(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
        key: String,
        value: String,
    ) -> Result<(), Error> {
        metadata::do_set_metadata(&env, subscription_id, &authorizer, key, value)
    }

    /// Delete a metadata key from a subscription.
    ///
    /// Authorization: subscriber or merchant.
    pub fn delete_metadata(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
        key: String,
    ) -> Result<(), Error> {
        metadata::do_delete_metadata(&env, subscription_id, &authorizer, key)
    }

    /// Get a metadata value by key.
    pub fn get_metadata(env: Env, subscription_id: u32, key: String) -> Result<String, Error> {
        metadata::do_get_metadata(&env, subscription_id, key)
    }

    /// List all metadata keys for a subscription.
    pub fn list_metadata_keys(env: Env, subscription_id: u32) -> Result<Vec<String>, Error> {
        metadata::do_list_metadata_keys(&env, subscription_id)
    }

    // ── Blocklist ──────────────────────────────────────────────────────────────

    pub fn add_to_blocklist(
        env: Env,
        authorizer: Address,
        subscriber: Address,
        reason: Option<String>,
    ) -> Result<(), Error> {
        blocklist::do_add_to_blocklist(&env, authorizer, subscriber, reason)
    }

    pub fn remove_from_blocklist(
        env: Env,
        admin: Address,
        subscriber: Address,
    ) -> Result<(), Error> {
        blocklist::do_remove_from_blocklist(&env, admin, subscriber)
    }

    pub fn get_blocklist_entry(env: Env, subscriber: Address) -> Result<BlocklistEntry, Error> {
        blocklist::get_blocklist_entry(&env, subscriber)
    }

    pub fn is_blocklisted(env: Env, subscriber: Address) -> bool {
        blocklist::is_blocklisted(&env, &subscriber)
    }

    /// Set global configuration for a merchant.
    ///
    /// Authorization: merchant.
    pub fn set_merchant_config(
        env: Env,
        merchant: Address,
        fee_address: Option<Address>,
        redirect_url: String,
        is_paused: bool,
    ) -> Result<(), Error> {
        let config = crate::types::MerchantConfig {
            fee_address,
            redirect_url,
            is_paused,
        };
        merchant::set_merchant_config(&env, merchant, config)
    }

    /// Get the global configuration for a merchant.
    pub fn get_merchant_config(
        env: Env,
        merchant: Address,
    ) -> Option<crate::types::MerchantConfig> {
        merchant::get_merchant_config(&env, merchant)
    }
}
