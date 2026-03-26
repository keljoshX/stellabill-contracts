//! Read-only entrypoints and helpers: get_subscription, estimate_topup, list_subscriptions_by_subscriber.
//!
//! **PRs that only add or change read-only/query behavior should edit this file only.**

use crate::types::{CapInfo, DataKey, Error, NextChargeInfo, Subscription, SubscriptionStatus};
use soroban_sdk::{contracttype, Address, Env, Symbol, Vec};

pub fn get_subscription(env: &Env, subscription_id: u32) -> Result<Subscription, Error> {
    env.storage()
        .instance()
        .get(&subscription_id)
        .ok_or(Error::NotFound)
}

pub fn estimate_topup_for_intervals(
    env: &Env,
    subscription_id: u32,
    num_intervals: u32,
) -> Result<i128, Error> {
    let sub = get_subscription(env, subscription_id)?;

    if num_intervals == 0 {
        return Ok(0);
    }

    let intervals_i128: i128 = num_intervals.into();
    let required = sub
        .amount
        .checked_mul(intervals_i128)
        .ok_or(Error::Overflow)?;

    let topup = required
        .checked_sub(sub.prepaid_balance)
        .unwrap_or(0)
        .max(0);
    Ok(topup)
}

/// Returns subscriptions for a merchant, paginated by offset.
pub fn get_subscriptions_by_merchant(
    env: &Env,
    merchant: Address,
    start: u32,
    limit: u32,
) -> Vec<Subscription> {
    let key = DataKey::MerchantSubs(merchant);
    let ids: Vec<u32> = env.storage().instance().get(&key).unwrap_or(Vec::new(env));

    let len = ids.len();
    if start >= len || limit == 0 {
        return Vec::new(env);
    }

    let end = if start + limit > len {
        len
    } else {
        start + limit
    };

    let mut result = Vec::new(env);
    let mut i = start;
    while i < end {
        let sub_id = ids.get(i).unwrap();
        if let Some(sub) = env.storage().instance().get::<u32, Subscription>(&sub_id) {
            result.push_back(sub);
        }
        i += 1;
    }
    result
}

/// Returns the number of subscriptions for a given merchant.
pub fn get_merchant_subscription_count(env: &Env, merchant: Address) -> u32 {
    let key = DataKey::MerchantSubs(merchant);
    let ids: Vec<u32> = env.storage().instance().get(&key).unwrap_or(Vec::new(env));
    ids.len()
}

/// Computes the estimated next charge timestamp for a subscription.
pub fn compute_next_charge_info(subscription: &Subscription) -> NextChargeInfo {
    let next_charge_timestamp = subscription
        .last_payment_timestamp
        .saturating_add(subscription.interval_seconds);

    let is_charge_expected = match subscription.status {
        SubscriptionStatus::Active => true,
        SubscriptionStatus::InsufficientBalance => true,
        SubscriptionStatus::GracePeriod => true,
        SubscriptionStatus::Paused => false,
        SubscriptionStatus::Cancelled => false,
    };

    NextChargeInfo {
        next_charge_timestamp,
        is_charge_expected,
    }
}

/// Returns lifetime cap information for a subscription.
pub fn get_cap_info(env: &Env, subscription_id: u32) -> Result<CapInfo, Error> {
    let sub = get_subscription(env, subscription_id)?;

    let (remaining_cap, cap_reached) = match sub.lifetime_cap {
        Some(cap) => {
            let remaining = cap.saturating_sub(sub.lifetime_charged).max(0);
            (Some(remaining), sub.lifetime_charged >= cap)
        }
        None => (None, false),
    };

    Ok(CapInfo {
        lifetime_cap: sub.lifetime_cap,
        lifetime_charged: sub.lifetime_charged,
        remaining_cap,
        cap_reached,
    })
}

/// Result of a paginated query for subscriptions by subscriber.
#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionsPage {
    pub subscription_ids: Vec<u32>,
    pub next_start_id: Option<u32>,
}

/// Get all subscription IDs for a given subscriber with pagination support.
pub fn list_subscriptions_by_subscriber(
    env: &Env,
    subscriber: Address,
    start_from_id: u32,
    limit: u32,
) -> Result<SubscriptionsPage, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidInput);
    }

    let next_id_key = Symbol::new(env, "next_id");
    let next_id: u32 = env.storage().instance().get(&next_id_key).unwrap_or(0);

    let mut subscription_ids = Vec::new(env);
    let mut next_start_id = None;

    for id in start_from_id..next_id {
        if let Some(sub) = env.storage().instance().get::<u32, Subscription>(&id) {
            if sub.subscriber == subscriber {
                if subscription_ids.len() < limit {
                    subscription_ids.push_back(id);
                } else {
                    next_start_id = Some(id);
                    break;
                }
            }
        }
    }

    Ok(SubscriptionsPage {
        subscription_ids,
        next_start_id,
    })
}