//! Single charge logic (no auth). Used by charge_subscription and batch_charge.
//!
//! Charge runs only when status is Active or GracePeriod. On insufficient balance the
//! function returns an error without persisting failure-path state mutations, so
//! batch and single-charge entrypoints observe the same ledger semantics.
//! On lifetime cap exhaustion the subscription is cancelled (terminal state).
//!
//! See `docs/subscription_lifecycle.md` for lifecycle details.
//! See `docs/lifetime_caps.md` for cap enforcement semantics.
//!
//! **PRs that only change how one subscription is charged should edit this file only.**

#![allow(dead_code)]

use crate::queries::get_subscription;
use crate::safe_math::safe_sub_balance;
use crate::state_machine::validate_status_transition;
use crate::statements::append_statement;
use crate::types::{
    BillingChargeKind, Error, LifetimeCapReachedEvent, SubscriptionChargedEvent, SubscriptionStatus,
};
use soroban_sdk::{symbol_short, Env, Symbol};

const KEY_CHARGED_PERIOD: Symbol = symbol_short!("cp");
const KEY_IDEM: Symbol = symbol_short!("idem");

fn charged_period_key(subscription_id: u32) -> (Symbol, u32) {
    (KEY_CHARGED_PERIOD, subscription_id)
}

fn idem_key(subscription_id: u32) -> (Symbol, u32) {
    (KEY_IDEM, subscription_id)
}

/// Performs a single interval-based charge with optional replay protection.
pub fn charge_one(
    env: &Env,
    subscription_id: u32,
    now: u64,
    idempotency_key: Option<soroban_sdk::BytesN<32>>,
) -> Result<(), Error> {
    let mut sub = get_subscription(env, subscription_id)?;
    let charge_amount = crate::oracle::resolve_charge_amount(env, &sub)?;

    if sub.status != SubscriptionStatus::Active && sub.status != SubscriptionStatus::GracePeriod {
        return Err(Error::NotActive);
    }

    let period_index = now / sub.interval_seconds;

    // Idempotent return: same idempotency key already processed
    if let Some(ref k) = idempotency_key {
        if let Some(stored) = env
            .storage()
            .instance()
            .get::<_, soroban_sdk::BytesN<32>>(&idem_key(subscription_id))
        {
            if stored == *k {
                return Ok(());
            }
        }
    }

    // Replay: already charged for this billing period
    if let Some(stored_period) = env
        .storage()
        .instance()
        .get::<_, u64>(&charged_period_key(subscription_id))
    {
        if period_index <= stored_period {
            return Err(Error::Replay);
        }
    }

    let next_allowed = sub
        .last_payment_timestamp
        .checked_add(sub.interval_seconds)
        .ok_or(Error::Overflow)?;
    if now < next_allowed {
        return Err(Error::IntervalNotElapsed);
    }

    // -- Lifetime cap pre-check -----------------------------------------------
    if let Some(cap) = sub.lifetime_cap {
        let remaining = cap.checked_sub(sub.lifetime_charged).unwrap_or(0).max(0);

        if remaining == 0 || charge_amount > remaining {
            // Cap already exhausted or this charge would exceed it — cancel.
            validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
            sub.status = SubscriptionStatus::Cancelled;
            env.storage().instance().set(&subscription_id, &sub);

            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                },
            );

            return Ok(());
        }
    }

    let storage = env.storage().instance();

    match safe_sub_balance(sub.prepaid_balance, charge_amount) {
        Ok(new_balance) => {
            sub.prepaid_balance = new_balance;
            crate::merchant::credit_merchant_balance_for_token(
                env,
                &sub.merchant,
                &sub.token,
                charge_amount,
            )?;
            sub.last_payment_timestamp = now;

            sub.lifetime_charged = sub
                .lifetime_charged
                .checked_add(charge_amount)
                .ok_or(Error::Overflow)?;

            // Recover from grace period on successful charge
            if sub.status == SubscriptionStatus::GracePeriod {
                validate_status_transition(&sub.status, &SubscriptionStatus::Active)?;
                sub.status = SubscriptionStatus::Active;
            }

            // Check if cap is now exactly reached -- auto-cancel
            let cap_reached = sub
                .lifetime_cap
                .map(|cap| sub.lifetime_charged >= cap)
                .unwrap_or(false);

            if cap_reached {
                validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
                sub.status = SubscriptionStatus::Cancelled;
            }

            storage.set(&subscription_id, &sub);
            append_statement(
                env,
                subscription_id,
                charge_amount,
                sub.merchant.clone(),
                BillingChargeKind::Interval,
                next_allowed.saturating_sub(sub.interval_seconds),
                now,
            );

            // Record charged period and optional idempotency key
            storage.set(&charged_period_key(subscription_id), &period_index);
            if let Some(k) = idempotency_key {
                storage.set(&idem_key(subscription_id), &k);
            }

            env.events().publish(
                (symbol_short!("charged"),),
                SubscriptionChargedEvent {
                    subscription_id,
                    merchant: sub.merchant.clone(),
                    amount: charge_amount,
                    lifetime_charged: sub.lifetime_charged,
                },
            );

            if cap_reached {
                if let Some(cap) = sub.lifetime_cap {
                    env.events().publish(
                        (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                        LifetimeCapReachedEvent {
                            subscription_id,
                            lifetime_cap: cap,
                            lifetime_charged: sub.lifetime_charged,
                            timestamp: now,
                        },
                    );
                }
            }

            Ok(())
        }
        Err(_) => {
            let grace_duration = crate::admin::get_grace_period(env).unwrap_or(0);
            let grace_expires = next_allowed
                .checked_add(grace_duration)
                .ok_or(Error::Overflow)?;

            if grace_duration > 0 && now < grace_expires {
                Err(Error::InsufficientBalance)
            } else {
                Err(Error::InsufficientBalance)
            }
        }
    }
}

/// Debit a metered `usage_amount` from a subscription's prepaid balance.
pub fn charge_usage_one(env: &Env, subscription_id: u32, usage_amount: i128) -> Result<(), Error> {
    let mut sub = get_subscription(env, subscription_id)?;

    if sub.status != SubscriptionStatus::Active {
        return Err(Error::NotActive);
    }

    if !sub.usage_enabled {
        return Err(Error::UsageNotEnabled);
    }

    if usage_amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    if sub.prepaid_balance < usage_amount {
        return Err(Error::InsufficientPrepaidBalance);
    }

    // -- Lifetime cap pre-check -----------------------------------------------
    if let Some(cap) = sub.lifetime_cap {
        let new_charged = sub
            .lifetime_charged
            .checked_add(usage_amount)
            .ok_or(Error::Overflow)?;
        if new_charged > cap {
            validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
            sub.status = SubscriptionStatus::Cancelled;
            env.storage().instance().set(&subscription_id, &sub);

            let now = env.ledger().timestamp();
            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                },
            );

            return Ok(());
        }
        sub.lifetime_charged = new_charged;
    }

    sub.prepaid_balance = sub
        .prepaid_balance
        .checked_sub(usage_amount)
        .ok_or(Error::Overflow)?;

    if sub.prepaid_balance == 0 {
        validate_status_transition(&sub.status, &SubscriptionStatus::InsufficientBalance)?;
        sub.status = SubscriptionStatus::InsufficientBalance;
    }

    let cap_reached = sub
        .lifetime_cap
        .map(|cap| sub.lifetime_charged >= cap)
        .unwrap_or(false);

    if cap_reached {
        validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
        sub.status = SubscriptionStatus::Cancelled;

        if let Some(cap) = sub.lifetime_cap {
            let now = env.ledger().timestamp();
            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                },
            );
        }
    }

    env.storage().instance().set(&subscription_id, &sub);
    append_statement(
        env,
        subscription_id,
        usage_amount,
        sub.merchant.clone(),
        BillingChargeKind::Usage,
        env.ledger().timestamp(),
        env.ledger().timestamp(),
    );
    Ok(())
}
