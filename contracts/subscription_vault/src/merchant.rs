//! Merchant payout and accumulated USDC tracking entrypoints.
//!
//! # Reentrancy Protection
//!
//! This module contains a critical external call: `withdraw_merchant_funds` transfers
//! USDC tokens to the merchant via `token.transfer()`. The implementation follows the
//! **Checks-Effects-Interactions (CEI)** pattern to prevent reentrancy attacks:
//!
//! 1. **Checks**: Validate merchant authorization and sufficient balance
//! 2. **Effects**: Update internal merchant balance in contract storage
//! 3. **Interactions**: Call token.transfer() AFTER state is consistent
//!
//! See `docs/reentrancy.md` for details on the reentrancy threat model and mitigation.

use crate::safe_math::{safe_sub_balance, validate_non_negative};
use crate::types::{Error, MerchantConfig, MerchantWithdrawalEvent};
use soroban_sdk::{token, Address, Env, Symbol};

fn merchant_balance_key(
    env: &Env,
    merchant: &Address,
    token: &Address,
) -> (Symbol, Address, Address) {
    (
        Symbol::new(env, "merchant_balance"),
        merchant.clone(),
        token.clone(),
    )
}

pub fn get_merchant_token_earnings(
    env: &Env,
    merchant: &Address,
    token: &Address,
) -> TokenEarnings {
    let key = DataKey::MerchantEarnings(merchant.clone(), token.clone());
    env.storage().instance().get(&key).unwrap_or(TokenEarnings {
        accruals: AccruedTotals {
            interval: 0,
            usage: 0,
            one_off: 0,
        },
        withdrawals: 0,
        refunds: 0,
    })
}

fn set_merchant_token_earnings(
    env: &Env,
    merchant: &Address,
    token: &Address,
    earnings: &TokenEarnings,
) {
    let key = DataKey::MerchantEarnings(merchant.clone(), token.clone());
    env.storage().instance().set(&key, earnings);
}

fn add_merchant_token(env: &Env, merchant: &Address, token: &Address) {
    let key = DataKey::MerchantTokens(merchant.clone());
    let mut tokens: Vec<Address> = env.storage().instance().get(&key).unwrap_or(Vec::new(env));
    if !tokens.contains(token) {
        tokens.push_back(token.clone());
        env.storage().instance().set(&key, &tokens);
    }
}

pub fn get_merchant_total_earnings(env: &Env, merchant: &Address) -> Vec<(Address, TokenEarnings)> {
    let key = DataKey::MerchantTokens(merchant.clone());
    let tokens: Vec<Address> = env.storage().instance().get(&key).unwrap_or(Vec::new(env));
    let mut result = Vec::new(env);
    for token in tokens.iter() {
        let earnings = get_merchant_token_earnings(env, merchant, &token);
        result.push_back((token, earnings));
    }
    result
}

pub fn get_reconciliation_snapshot(
    env: &Env,
    merchant: &Address,
) -> Vec<TokenReconciliationSnapshot> {
    let key = DataKey::MerchantTokens(merchant.clone());
    let tokens: Vec<Address> = env.storage().instance().get(&key).unwrap_or(Vec::new(env));
    let mut result = Vec::new(env);

    for token in tokens.iter() {
        let earnings = get_merchant_token_earnings(env, merchant, &token);
        let total_accruals = earnings
            .accruals
            .interval
            .checked_add(earnings.accruals.usage)
            .unwrap_or(0)
            .checked_add(earnings.accruals.one_off)
            .unwrap_or(0);

        let computed_balance = total_accruals
            .checked_sub(earnings.withdrawals)
            .unwrap_or(0)
            .checked_sub(earnings.refunds)
            .unwrap_or(0);

        result.push_back(TokenReconciliationSnapshot {
            token: token.clone(),
            total_accruals,
            total_withdrawals: earnings.withdrawals,
            total_refunds: earnings.refunds,
            computed_balance,
        });
    }
    result
}

pub fn get_merchant_balance(env: &Env, merchant: &Address) -> i128 {
    if let Ok(token_addr) = crate::admin::get_token(env) {
        return get_merchant_balance_by_token(env, merchant, &token_addr);
    }
    0
}

pub fn get_merchant_balance_by_token(env: &Env, merchant: &Address, token: &Address) -> i128 {
    let key = merchant_balance_key(env, merchant, token);
    env.storage().instance().get(&key).unwrap_or(0i128)
}

fn set_merchant_balance(env: &Env, merchant: &Address, token: &Address, balance: &i128) {
    let key = merchant_balance_key(env, merchant, token);
    env.storage().instance().set(&key, balance);
}

/// Credit merchant balance (used when subscription charges process).
#[allow(dead_code)]
pub fn credit_merchant_balance(
    env: &Env,
    merchant: &Address,
    amount: i128,
    kind: BillingChargeKind,
) -> Result<(), Error> {
    let token_addr = crate::admin::get_token(env)?;
    credit_merchant_balance_for_token(env, merchant, &token_addr, amount, kind)
}

pub fn credit_merchant_balance_for_token(
    env: &Env,
    merchant: &Address,
    token_addr: &Address,
    amount: i128,
    kind: BillingChargeKind,
) -> Result<(), Error> {
    validate_non_negative(amount)?;

    // Update simple balance
    let current = get_merchant_balance_by_token(env, merchant, token_addr);
    let new_balance = current.checked_add(amount).ok_or(Error::Overflow)?;
    set_merchant_balance(env, merchant, token_addr, &new_balance);

    // Update earnings struct
    let mut earnings = get_merchant_token_earnings(env, merchant, token_addr);
    match kind {
        BillingChargeKind::Interval => {
            earnings.accruals.interval = earnings
                .accruals
                .interval
                .checked_add(amount)
                .ok_or(Error::Overflow)?
        }
        BillingChargeKind::Usage => {
            earnings.accruals.usage = earnings
                .accruals
                .usage
                .checked_add(amount)
                .ok_or(Error::Overflow)?
        }
        BillingChargeKind::OneOff => {
            earnings.accruals.one_off = earnings
                .accruals
                .one_off
                .checked_add(amount)
                .ok_or(Error::Overflow)?
        }
    }
    set_merchant_token_earnings(env, merchant, token_addr, &earnings);
    add_merchant_token(env, merchant, token_addr);

    Ok(())
}

/// Withdraw accumulated USDC from prior subscription charges to the merchant address.
///
/// **Reentrancy Protection**: This function follows the Checks-Effects-Interactions (CEI) pattern:
/// 1. All validation happens first (checks)
/// 2. Internal state is updated before any external calls (effects)
/// 3. External token transfer happens last (interactions)
///
/// This ordering ensures that if the token contract attempts a callback into our contract,
/// our internal state will already be consistent and the merchant balance will be correct.
pub fn withdraw_merchant_funds(env: &Env, merchant: Address, amount: i128) -> Result<(), Error> {
    let token_addr = crate::admin::get_token(env)?;
    withdraw_merchant_funds_for_token(env, merchant, token_addr, amount)
}

pub fn withdraw_merchant_funds_for_token(
    env: &Env,
    merchant: Address,
    token_addr: Address,
    amount: i128,
) -> Result<(), Error> {
    merchant.require_auth();
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }
    if !crate::admin::is_token_accepted(env, &token_addr) {
        return Err(Error::InvalidInput);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // CHECKS: Validate all preconditions before any state mutations
    // ──────────────────────────────────────────────────────────────────────────
    let current = get_merchant_balance_by_token(env, &merchant, &token_addr);
    if current == 0 {
        return Err(Error::NotFound);
    }
    if amount > current {
        return Err(Error::InsufficientBalance);
    }

    let token_client = token::Client::new(env, &token_addr);
    let contract = env.current_contract_address();
    let contract_balance = token_client.balance(&contract);
    if contract_balance < amount {
        return Err(Error::InsufficientBalance);
    }

    let new_balance = safe_sub_balance(current, amount)?;

    // ──────────────────────────────────────────────────────────────────────────
    // EFFECTS: Update internal state before external interactions (CEI pattern)
    // ──────────────────────────────────────────────────────────────────────────
    set_merchant_balance(env, &merchant, &token_addr, &new_balance);
    env.events().publish(
        (
            Symbol::new(env, "withdrawn"),
            merchant.clone(),
            token_addr.clone(),
        ),
        MerchantWithdrawalEvent {
            merchant: merchant.clone(),
            token: token_addr.clone(),
            amount,
            remaining_balance: new_balance,
        },
    );

    // ──────────────────────────────────────────────────────────────────────────
    // INTERACTIONS: Only after internal state is consistent, call token contract
    // This ensures that even if token contract calls back, our state is correct
    // ──────────────────────────────────────────────────────────────────────────
    token_client.transfer(&contract, &merchant, &amount);

    Ok(())
}

pub fn merchant_refund(
    env: &Env,
    merchant: Address,
    subscriber: Address,
    token_addr: Address,
    amount: i128,
) -> Result<(), Error> {
    merchant.require_auth();
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    let current = get_merchant_balance_by_token(env, &merchant, &token_addr);
    if current == 0 {
        return Err(Error::NotFound);
    }
    if amount > current {
        return Err(Error::InsufficientBalance);
    }

    let new_balance = current.checked_sub(amount).ok_or(Error::Overflow)?;

    // EFFECTS
    set_merchant_balance(env, &merchant, &token_addr, &new_balance);

    let mut earnings = get_merchant_token_earnings(env, &merchant, &token_addr);
    earnings.refunds = earnings
        .refunds
        .checked_add(amount)
        .ok_or(Error::Overflow)?;
    set_merchant_token_earnings(env, &merchant, &token_addr, &earnings);

    env.events().publish(
        (Symbol::new(env, "merchant_refund"), merchant.clone()),
        crate::types::MerchantRefundEvent {
            merchant,
            subscriber: subscriber.clone(),
            token: token_addr.clone(),
            amount,
        },
    );

    // INTERACTIONS
    let token_client = token::Client::new(env, &token_addr);
    token_client.transfer(&env.current_contract_address(), &subscriber, &amount);

    Ok(())
}
