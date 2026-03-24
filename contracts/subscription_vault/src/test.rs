use crate::{
    can_transition, compute_next_charge_info, get_allowed_transitions, validate_status_transition,
    Error, OraclePrice, RecoveryReason, Subscription, SubscriptionStatus, SubscriptionVault,
    SubscriptionVaultClient, MAX_SUBSCRIPTION_ID,
};
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::{contract, contractimpl, Address, Env, String, Symbol, Vec as SorobanVec};

extern crate alloc;
use alloc::format;

// -- constants ----------------------------------------------------------------
const T0: u64 = 1_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60; // 30 days
const AMOUNT: i128 = 10_000_000; // 10 USDC (6 decimals)
const PREPAID: i128 = 50_000_000; // 50 USDC

// -- helpers ------------------------------------------------------------------

fn create_token_and_mint(env: &Env, recipient: &Address, amount: i128) -> Address {
    let token_admin = Address::generate(env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let token_client = soroban_sdk::token::StellarAssetClient::new(env, &token_addr);
    token_client.mint(recipient, &amount);
    token_addr
}

/// Standard setup: mock auth, register contract, init with real token + 7-day grace.
fn setup_test_env() -> (Env, SubscriptionVaultClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let min_topup = 1_000_000i128; // 1 USDC
    client.init(&token, &6, &admin, &min_topup, &(7 * 24 * 60 * 60));

    (env, client, token, admin)
}

/// Helper used by reentrancy tests: returns (client, token, admin) with env pre-configured.
fn setup_contract(env: &Env) -> (SubscriptionVaultClient<'_>, Address, Address) {
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
    (client, token, admin)
}

/// Create a test subscription, then patch its status for direct-manipulation tests.
fn create_test_subscription(
    env: &Env,
    client: &SubscriptionVaultClient,
    status: SubscriptionStatus,
) -> (u32, Address, Address) {
    let subscriber = Address::generate(env);
    let merchant = Address::generate(env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    if status != SubscriptionStatus::Active {
        let mut sub = client.get_subscription(&id);
        sub.status = status;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
    }
    (id, subscriber, merchant)
}

/// Seed a subscription with a known prepaid balance directly in storage.
fn seed_balance(env: &Env, client: &SubscriptionVaultClient, id: u32, balance: i128) {
    let mut sub = client.get_subscription(&id);
    sub.prepaid_balance = balance;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });
}

/// Seed the `next_id` counter to an arbitrary value.
fn seed_counter(env: &Env, contract_id: &Address, value: u32) {
    env.as_contract(contract_id, || {
        env.storage()
            .instance()
            .set(&soroban_sdk::Symbol::new(env, "next_id"), &value);
    });
}

#[contract]
struct MockOracle;

#[contractimpl]
impl MockOracle {
    pub fn set_price(env: Env, price: i128, timestamp: u64) {
        env.storage().instance().set(
            &Symbol::new(&env, "price"),
            &OraclePrice { price, timestamp },
        );
    }

    pub fn latest_price(env: Env) -> OraclePrice {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "price"))
            .unwrap_or(OraclePrice {
                price: 0,
                timestamp: 0,
            })
    }
}

// ── State Machine Helper Tests ─────────────────────────────────────────────────

#[test]
fn test_validate_status_transition_same_status_is_allowed() {
    assert!(
        validate_status_transition(&SubscriptionStatus::Active, &SubscriptionStatus::Active)
            .is_ok()
    );
    assert!(
        validate_status_transition(&SubscriptionStatus::Paused, &SubscriptionStatus::Paused)
            .is_ok()
    );
    assert!(validate_status_transition(
        &SubscriptionStatus::Cancelled,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert!(validate_status_transition(
        &SubscriptionStatus::InsufficientBalance,
        &SubscriptionStatus::InsufficientBalance
    )
    .is_ok());
}

#[test]
fn test_validate_active_transitions() {
    assert!(
        validate_status_transition(&SubscriptionStatus::Active, &SubscriptionStatus::Paused)
            .is_ok()
    );
    assert!(validate_status_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert!(validate_status_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::InsufficientBalance
    )
    .is_ok());
}

#[test]
fn test_validate_paused_transitions() {
    assert!(
        validate_status_transition(&SubscriptionStatus::Paused, &SubscriptionStatus::Active)
            .is_ok()
    );
    assert!(validate_status_transition(
        &SubscriptionStatus::Paused,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert_eq!(
        validate_status_transition(
            &SubscriptionStatus::Paused,
            &SubscriptionStatus::InsufficientBalance
        ),
        Err(Error::InvalidStatusTransition)
    );
}

#[test]
fn test_validate_insufficient_balance_transitions() {
    assert!(validate_status_transition(
        &SubscriptionStatus::InsufficientBalance,
        &SubscriptionStatus::Active
    )
    .is_ok());
    assert!(validate_status_transition(
        &SubscriptionStatus::InsufficientBalance,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert_eq!(
        validate_status_transition(
            &SubscriptionStatus::InsufficientBalance,
            &SubscriptionStatus::Paused
        ),
        Err(Error::InvalidStatusTransition)
    );
}

#[test]
fn test_validate_cancelled_transitions_all_blocked() {
    assert_eq!(
        validate_status_transition(&SubscriptionStatus::Cancelled, &SubscriptionStatus::Active),
        Err(Error::InvalidStatusTransition)
    );
    assert_eq!(
        validate_status_transition(&SubscriptionStatus::Cancelled, &SubscriptionStatus::Paused),
        Err(Error::InvalidStatusTransition)
    );
    assert_eq!(
        validate_status_transition(
            &SubscriptionStatus::Cancelled,
            &SubscriptionStatus::InsufficientBalance
        ),
        Err(Error::InvalidStatusTransition)
    );
}

#[test]
fn test_can_transition_helper() {
    assert!(can_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::Paused
    ));
    assert!(can_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::Cancelled
    ));
    assert!(can_transition(
        &SubscriptionStatus::Paused,
        &SubscriptionStatus::Active
    ));
    assert!(!can_transition(
        &SubscriptionStatus::Cancelled,
        &SubscriptionStatus::Active
    ));
    assert!(!can_transition(
        &SubscriptionStatus::Cancelled,
        &SubscriptionStatus::Paused
    ));
    assert!(!can_transition(
        &SubscriptionStatus::Paused,
        &SubscriptionStatus::InsufficientBalance
    ));
}

#[test]
fn test_get_allowed_transitions() {
    let active_targets = get_allowed_transitions(&SubscriptionStatus::Active);
    assert!(active_targets.contains(&SubscriptionStatus::Paused));
    assert!(active_targets.contains(&SubscriptionStatus::Cancelled));
    assert!(active_targets.contains(&SubscriptionStatus::InsufficientBalance));

    let paused_targets = get_allowed_transitions(&SubscriptionStatus::Paused);
    assert_eq!(paused_targets.len(), 2);
    assert!(paused_targets.contains(&SubscriptionStatus::Active));
    assert!(paused_targets.contains(&SubscriptionStatus::Cancelled));

    assert_eq!(
        get_allowed_transitions(&SubscriptionStatus::Cancelled).len(),
        0
    );

    let ib_targets = get_allowed_transitions(&SubscriptionStatus::InsufficientBalance);
    assert_eq!(ib_targets.len(), 2);
}

// -- Contract Lifecycle Tests -------------------------------------------------

#[test]
fn test_pause_subscription_from_active() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_pause_subscription_from_cancelled_should_fail() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.pause_subscription(&id, &subscriber);
}

#[test]
fn test_pause_subscription_from_paused_is_idempotent() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
fn test_cancel_subscription_from_active() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled
    );
}

#[test]
fn test_cancel_subscription_from_paused() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    client.cancel_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled
    );
}

#[test]
fn test_cancel_subscription_from_cancelled_is_idempotent() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.cancel_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled
    );
}

#[test]
fn test_resume_subscription_from_paused() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    client.resume_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Active
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_resume_subscription_from_cancelled_should_fail() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.resume_subscription(&id, &subscriber);
}

#[test]
fn test_full_lifecycle_active_pause_resume() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
    client.resume_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Active
    );
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
fn test_all_valid_transitions_coverage() {
    // Active -> Paused
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.pause_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Paused
        );
    }
    // Active -> Cancelled
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.cancel_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Cancelled
        );
    }
    // Active -> InsufficientBalance (direct storage patch)
    {
        let (env, client, _, _) = setup_test_env();
        let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
        let mut sub = client.get_subscription(&id);
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::InsufficientBalance
        );
    }
    // Paused -> Active
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.pause_subscription(&id, &subscriber);
        client.resume_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Active
        );
    }
    // Paused -> Cancelled
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.pause_subscription(&id, &subscriber);
        client.cancel_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Cancelled
        );
    }
    // InsufficientBalance -> Active
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        let mut sub = client.get_subscription(&id);
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
        client.resume_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Active
        );
    }
    // InsufficientBalance -> Cancelled
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        let mut sub = client.get_subscription(&id);
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
        client.cancel_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Cancelled
        );
    }
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_invalid_cancelled_to_active() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.resume_subscription(&id, &subscriber);
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_invalid_insufficient_balance_to_paused() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let mut sub = client.get_subscription(&id);
    sub.status = SubscriptionStatus::InsufficientBalance;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });
    client.pause_subscription(&id, &subscriber);
}

// -- Subscription struct tests ------------------------------------------------

#[test]
fn test_subscription_struct_status_field() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: 100_000_000,
        interval_seconds: 30 * 24 * 60 * 60,
        last_payment_timestamp: 0,
        status: SubscriptionStatus::Active,
        prepaid_balance: 500_000_000,
        usage_enabled: false,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    assert_eq!(sub.status, SubscriptionStatus::Active);
    assert_eq!(sub.lifetime_cap, None);
    assert_eq!(sub.lifetime_charged, 0);
}

#[test]
fn test_subscription_struct_with_lifetime_cap() {
    let env = Env::default();
    let cap = 120_000_000i128; // 120 USDC
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: 10_000_000,
        interval_seconds: 30 * 24 * 60 * 60,
        last_payment_timestamp: 0,
        status: SubscriptionStatus::Active,
        prepaid_balance: 50_000_000,
        usage_enabled: false,
        lifetime_cap: Some(cap),
        lifetime_charged: 0,
    };
    assert_eq!(sub.lifetime_cap, Some(cap));
    assert_eq!(sub.lifetime_charged, 0);
}

// -- Contract Charging Tests --------------------------------------------------

#[test]
fn test_charge_subscription_basic() {
    let (env, client, _, admin) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, PREPAID - AMOUNT);
    assert_eq!(sub.lifetime_charged, AMOUNT);
}

#[test]
#[should_panic(expected = "Error(Contract, #1002)")]
fn test_charge_subscription_paused_fails() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);
    client.pause_subscription(&id, &subscriber);
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);
}

#[test]
fn test_charge_subscription_insufficient_balance_returns_error() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    // Do not fund - balance stays 0
    // Charge attempt after interval + grace period should return InsufficientBalance error.
    // NOTE: Soroban reverts all state changes when a contract call returns Err,
    // so the status transition to InsufficientBalance is rolled back on-chain.
    let grace_period = 7 * 24 * 60 * 60u64;
    env.ledger()
        .with_mut(|li| li.timestamp = T0 + INTERVAL + grace_period + 1);
    let result = client.try_charge_subscription(&id);
    assert!(result.is_err());
}

// -- ID limit test ------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #429)")]
fn test_subscription_limit_reached() {
    let (env, client, _, _) = setup_test_env();
    seed_counter(&env, &client.address, MAX_SUBSCRIPTION_ID);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
}

#[test]
fn test_cancel_subscription_unauthorized() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let other = Address::generate(&env);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &1000, &86400, &true, &None::<i128>);
    let result = client.try_cancel_subscription(&sub_id, &other);
    assert_eq!(result, Err(Ok(Error::Forbidden)));
}

#[test]
fn test_withdraw_subscriber_funds() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let token_contract = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token = soroban_sdk::token::Client::new(&env, &token_contract);
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token_contract);

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let vault_admin = Address::generate(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    client.init(
        &token_contract,
        &6,
        &vault_admin,
        &1000,
        &(7 * 24 * 60 * 60),
    );
    token_admin.mint(&subscriber, &5000);

    let sub_id =
        client.create_subscription(&subscriber, &merchant, &1000, &86400, &true, &None::<i128>);
    client.deposit_funds(&sub_id, &subscriber, &5000);
    client.cancel_subscription(&sub_id, &subscriber);
    client.withdraw_subscriber_funds(&sub_id, &subscriber);

    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.prepaid_balance, 0);
    assert_eq!(token.balance(&subscriber), 5000);
    assert_eq!(token.balance(&contract_id), 0);
}

// ── Min-Topup Enforcement Tests ────────────────────────────────────────────────

#[test]
fn test_min_topup_below_threshold() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let min_topup = 5_000_000i128;

    client.init(&token_addr, &6, &admin, &min_topup, &(7 * 24 * 60 * 60));
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &10_000_000i128,
        &(30 * 24 * 60 * 60),
        &false,
        &None::<i128>,
    );
    client.cancel_subscription(&id, &merchant);
    let result = client.try_deposit_funds(&id, &subscriber, &4_999_999);
    assert!(result.is_err());
}

#[test]
fn test_min_topup_exactly_at_threshold() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let min_topup = 5_000_000i128;

    client.init(&token_addr, &6, &admin, &min_topup, &(7 * 24 * 60 * 60));
    token_admin.mint(&subscriber, &min_topup);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &10_000_000i128,
        &(30 * 24 * 60 * 60),
        &false,
        &None::<i128>,
    );
    assert!(client
        .try_deposit_funds(&id, &subscriber, &min_topup)
        .is_ok());
}

// -- Deposit tests ------------------------------------------------------------

#[test]
fn test_min_topup_above_threshold() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let min_topup = 5_000_000i128;
    let deposit_amount = 10_000_000i128;

    client.init(&token_addr, &6, &admin, &min_topup, &(7 * 24 * 60 * 60));
    token_admin.mint(&subscriber, &deposit_amount);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &deposit_amount,
        &(30 * 24 * 60 * 60),
        &false,
        &None::<i128>,
    );
    assert!(client
        .try_deposit_funds(&id, &subscriber, &deposit_amount)
        .is_ok());
}

// ── Usage-charge tests ─────────────────────────────────────────────────────────

// -- Deposit tests ------------------------------------------------------------

#[test]
fn test_deposit_funds_basic() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&subscriber, &100_000_000);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &5_000_000);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 5_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #402)")]
fn test_deposit_funds_below_minimum() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    // min_topup is 1_000_000; try to deposit 500
    client.deposit_funds(&id, &subscriber, &500);
}

// -- Admin tests --------------------------------------------------------------

#[test]
fn test_rotate_admin() {
    let (env, client, _, admin) = setup_test_env();
    let new_admin = Address::generate(&env);
    client.rotate_admin(&admin, &new_admin);
    assert_eq!(client.get_admin(), new_admin);
}

#[test]
fn test_emergency_stop() {
    let (env, client, _, admin) = setup_test_env();
    assert!(!client.get_emergency_stop_status());
    client.enable_emergency_stop(&admin);
    assert!(client.get_emergency_stop_status());
    client.disable_emergency_stop(&admin);
    assert!(!client.get_emergency_stop_status());
}

#[test]
#[should_panic(expected = "Error(Contract, #1009)")]
fn test_create_subscription_blocked_by_emergency_stop() {
    let (env, client, _, admin) = setup_test_env();
    client.enable_emergency_stop(&admin);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
}

// -- Batch charge tests -------------------------------------------------------

#[test]
fn test_batch_charge() {
    let (env, client, _, admin) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id1, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let (id2, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id1, PREPAID);
    seed_balance(&env, &client, id2, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);

    let ids = SorobanVec::from_array(&env, [id1, id2]);
    let results = client.batch_charge(&ids);
    assert_eq!(results.len(), 2);
    assert!(results.get(0).unwrap().success);
    assert!(results.get(1).unwrap().success);
}

// -- Next charge info test ----------------------------------------------------

#[test]
fn test_next_charge_info() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let info = client.get_next_charge_info(&id);
    assert_eq!(info.next_charge_timestamp, T0 + INTERVAL);
    assert!(info.is_charge_expected);
}

// -- Compute next charge info (unit) ------------------------------------------

#[test]
fn test_compute_next_charge_info_active() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: T0,
        status: SubscriptionStatus::Active,
        prepaid_balance: 0,
        usage_enabled: false,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert_eq!(info.next_charge_timestamp, T0 + INTERVAL);
    assert!(info.is_charge_expected);
}

#[test]
fn test_compute_next_charge_info_paused() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: 2000,
        status: SubscriptionStatus::Paused,
        prepaid_balance: 50_000_000,
        usage_enabled: false,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(!info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, 2000 + INTERVAL);
}

#[test]
fn test_compute_next_charge_info_cancelled() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: T0,
        status: SubscriptionStatus::Cancelled,
        prepaid_balance: 0,
        usage_enabled: false,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(!info.is_charge_expected);
}

#[test]
fn test_compute_next_charge_info_insufficient_balance() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: 3000,
        status: SubscriptionStatus::InsufficientBalance,
        prepaid_balance: 1_000_000,
        usage_enabled: false,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(info.is_charge_expected);
}

#[test]
fn test_compute_next_charge_info_overflow_protection() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        token: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: 200,
        last_payment_timestamp: u64::MAX - 100,
        status: SubscriptionStatus::Active,
        prepaid_balance: 100_000_000,
        usage_enabled: false,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, u64::MAX);
}

// -- Replay protection --------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #1007)")]
fn test_replay_charge_same_period() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);
    // Second charge in same period should fail
    client.charge_subscription(&id);
}

// -- Recovery -----------------------------------------------------------------

#[test]
fn test_recover_stranded_funds() {
    let (env, client, _, admin) = setup_test_env();
    let recipient = Address::generate(&env);
    client.recover_stranded_funds(
        &admin,
        &recipient,
        &1_000_000,
        &RecoveryReason::AccidentalTransfer,
    );
    // No panic means success (actual transfer is TODO in admin.rs)
}

// -- Lifetime cap tests -------------------------------------------------------

#[test]
fn test_lifetime_cap_auto_cancel() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    // Cap = 2 * AMOUNT, so after 2 charges, should auto-cancel
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(2 * AMOUNT),
    );
    seed_balance(&env, &client, id, PREPAID);

    // First charge
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, AMOUNT);
    assert_eq!(sub.status, SubscriptionStatus::Active);

    // Second charge -> cap reached -> auto-cancel
    env.ledger()
        .with_mut(|li| li.timestamp = T0 + 2 * INTERVAL + 1);
    client.charge_subscription(&id);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, 2 * AMOUNT);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
}

#[test]
fn test_get_cap_info() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let cap = 100_000_000i128;
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(cap),
    );
    let info = client.get_cap_info(&id);
    assert_eq!(info.lifetime_cap, Some(cap));
    assert_eq!(info.lifetime_charged, 0);
    assert_eq!(info.remaining_cap, Some(cap));
    assert!(!info.cap_reached);
}

// -- Plan template tests ------------------------------------------------------

/// Plan template inherits lifetime_cap to subscriptions created from it.
#[test]
fn test_plan_template_inherits_lifetime_cap() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let cap = 50_000_000i128;
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &Some(cap));

    let template = client.get_plan_template(&plan_id);
    assert_eq!(template.lifetime_cap, Some(cap));
    assert_eq!(template.version, 1);
    assert_eq!(template.template_key, plan_id);

    let sub_id = client.create_subscription_from_plan(&subscriber, &plan_id);
    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.lifetime_cap, Some(cap));
    assert_eq!(sub.lifetime_charged, 0);
}

/// Plan template with no cap creates uncapped subscriptions.
#[test]
fn test_plan_template_no_cap_creates_uncapped_sub() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    let plan = client.get_plan_template(&plan_id);
    assert_eq!(plan.amount, AMOUNT);
    assert_eq!(plan.merchant, merchant);

    let sub_id = client.create_subscription_from_plan(&subscriber, &plan_id);
    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.amount, AMOUNT);
    assert_eq!(sub.merchant, merchant);
    assert_eq!(sub.subscriber, subscriber);
}

#[test]
fn test_plan_max_concurrent_subscriptions_enforced_per_subscriber() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    // Limit each subscriber to a single active subscription for this plan.
    client.set_plan_max_active_subs(&merchant, &plan_id, &1);

    // First subscription succeeds.
    let _sub1 = client.create_subscription_from_plan(&subscriber, &plan_id);

    // Second subscription for the same subscriber/plan is rejected.
    let result = client.try_create_subscription_from_plan(&subscriber, &plan_id);
    assert_eq!(result, Err(Ok(Error::MaxConcurrentSubscriptionsReached)));

    // Another subscriber is unaffected by this limit.
    let other_subscriber = Address::generate(&env);
    let _sub_other = client.create_subscription_from_plan(&other_subscriber, &plan_id);
}

#[test]
fn test_plan_max_concurrent_allows_new_after_cancellation() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    client.set_plan_max_active_subs(&merchant, &plan_id, &1);

    let sub1 = client.create_subscription_from_plan(&subscriber, &plan_id);
    client.cancel_subscription(&sub1, &subscriber);

    // Because only ACTIVE subscriptions are counted, a new subscription is allowed
    // after cancellation.
    let sub2 = client.create_subscription_from_plan(&subscriber, &plan_id);
    let sub = client.get_subscription(&sub2);
    assert_eq!(sub.status, SubscriptionStatus::Active);
}

#[test]
fn test_subscriber_credit_limit_blocks_new_subscription_creation() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    // Limit total exposure for this subscriber/token to a single interval amount.
    client.set_subscriber_credit_limit(&admin, &subscriber, &token, &AMOUNT);

    // First subscription fits entirely within the limit.
    let _sub1 =
        client.create_subscription(&subscriber, &merchant, &AMOUNT, &INTERVAL, &false, &None);

    // Second subscription would exceed credit limit (another interval liability).
    let result =
        client.try_create_subscription(&subscriber, &merchant, &AMOUNT, &INTERVAL, &false, &None);
    assert_eq!(result, Err(Ok(Error::CreditLimitExceeded)));
}

#[test]
fn test_subscriber_credit_limit_blocks_topup_when_exposure_exceeds_limit() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    // Exposure limit small enough that initial subscription fits, but top-up does not.
    let limit = AMOUNT + 5_000_000i128;
    client.set_subscriber_credit_limit(&admin, &subscriber, &token, &limit);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    // Deposit that would keep us under the limit succeeds.
    client.deposit_funds(&sub_id, &subscriber, &5_000_000i128);

    // Further deposit would push exposure over the limit and must be rejected.
    let result = client.try_deposit_funds(&sub_id, &subscriber, &1_000_000i128);
    assert_eq!(result, Err(Ok(Error::CreditLimitExceeded)));
}

#[test]
fn test_get_subscriber_credit_limit_and_exposure_views() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    // Default: no limit configured.
    assert_eq!(client.get_subscriber_credit_limit(&subscriber, &token), 0);

    client.set_subscriber_credit_limit(&admin, &subscriber, &token, &(AMOUNT * 10));

    // After creating a subscription, exposure reflects one interval liability and zero prepaid.
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let exposure = client.get_subscriber_exposure(&subscriber, &token);
    assert_eq!(exposure, AMOUNT);

    // After topping up, exposure increases by the deposited amount.
    client.deposit_funds(&sub_id, &subscriber, &5_000_000i128);
    let exposure_after_topup = client.get_subscriber_exposure(&subscriber, &token);
    assert_eq!(exposure_after_topup, AMOUNT + 5_000_000i128);
}

#[test]
fn test_partial_refund_debits_prepaid_and_transfers_tokens() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let token_contract = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token = soroban_sdk::token::Client::new(&env, &token_contract);
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token_contract);

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let vault_admin = Address::generate(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    client.init(
        &token_contract,
        &6,
        &vault_admin,
        &1_000_000i128,
        &(7 * 24 * 60 * 60),
    );

    // Seed subscriber with tokens and create a funded subscription.
    token_admin.mint(&subscriber, &50_000_000i128);
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&sub_id, &subscriber, &20_000_000i128);

    let balance_before = token.balance(&subscriber);
    let sub_before = client.get_subscription(&sub_id);
    assert_eq!(sub_before.prepaid_balance, 20_000_000i128);

    // Perform a partial refund of half the prepaid balance.
    client.partial_refund(&vault_admin, &sub_id, &subscriber, &10_000_000i128);

    let balance_after = token.balance(&subscriber);
    let sub_after = client.get_subscription(&sub_id);

    assert_eq!(sub_after.prepaid_balance, 10_000_000i128);
    assert_eq!(balance_after, balance_before + 10_000_000i128);
}

#[test]
fn test_partial_refund_rejects_invalid_amounts_and_auth() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let token_contract = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token_contract);

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let vault_admin = Address::generate(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    client.init(
        &token_contract,
        &6,
        &vault_admin,
        &1_000_000i128,
        &(7 * 24 * 60 * 60),
    );

    token_admin.mint(&subscriber, &50_000_000i128);
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&sub_id, &subscriber, &5_000_000i128);

    // Zero or negative refund amounts are rejected.
    let zero_res = client.try_partial_refund(&vault_admin, &sub_id, &subscriber, &0i128);
    assert_eq!(zero_res, Err(Ok(Error::InvalidAmount)));

    let negative_res = client.try_partial_refund(&vault_admin, &sub_id, &subscriber, &-1i128);
    assert_eq!(negative_res, Err(Ok(Error::InvalidAmount)));

    // Refund exceeding prepaid balance is rejected.
    let over_res = client.try_partial_refund(&vault_admin, &sub_id, &subscriber, &10_000_000i128);
    assert_eq!(over_res, Err(Ok(Error::InsufficientBalance)));

    // Non-admin cannot authorize partial refunds.
    let other_admin = Address::generate(&env);
    let unauth_res = client.try_partial_refund(&other_admin, &sub_id, &subscriber, &1_000_000i128);
    assert_eq!(unauth_res, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_update_plan_template_creates_new_version_and_preserves_old() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let cap = 50_000_000i128;
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &Some(cap));
    let original = client.get_plan_template(&plan_id);
    assert_eq!(original.version, 1);

    let new_amount = AMOUNT * 2;
    let new_interval = INTERVAL / 2;
    let new_plan_id = client.update_plan_template(
        &merchant,
        &plan_id,
        &new_amount,
        &new_interval,
        &true,
        &Some(cap),
    );

    // Old plan remains unchanged and addressable.
    let original_after = client.get_plan_template(&plan_id);
    assert_eq!(original_after.version, 1);
    assert_eq!(original_after.amount, AMOUNT);
    assert_eq!(original_after.interval_seconds, INTERVAL);
    assert!(!original_after.usage_enabled);

    // New plan has incremented version and updated fields, sharing template_key.
    let updated = client.get_plan_template(&new_plan_id);
    assert_eq!(updated.version, 2);
    assert_eq!(updated.template_key, original_after.template_key);
    assert_eq!(updated.amount, new_amount);
    assert_eq!(updated.interval_seconds, new_interval);
    assert!(updated.usage_enabled);
    assert_eq!(updated.lifetime_cap, Some(cap));
}

#[test]
fn test_migrate_subscription_to_new_plan_version() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let cap = 50_000_000i128;
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &Some(cap));
    let new_amount = AMOUNT * 3;
    let new_interval = INTERVAL / 3;
    let new_plan_id = client.update_plan_template(
        &merchant,
        &plan_id,
        &new_amount,
        &new_interval,
        &true,
        &Some(cap),
    );

    let sub_id = client.create_subscription_from_plan(&subscriber, &plan_id);
    let before = client.get_subscription(&sub_id);
    assert_eq!(before.amount, AMOUNT);
    assert_eq!(before.interval_seconds, INTERVAL);
    assert!(!before.usage_enabled);

    client.migrate_subscription_to_plan(&subscriber, &sub_id, &new_plan_id);

    let after = client.get_subscription(&sub_id);
    assert_eq!(after.amount, new_amount);
    assert_eq!(after.interval_seconds, new_interval);
    assert!(after.usage_enabled);
    // Lifetime tracking is preserved.
    assert_eq!(after.lifetime_charged, 0);
    assert_eq!(after.lifetime_cap, Some(cap));
}

#[test]
fn test_migrate_subscription_rejects_cross_template_family() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let plan_family_a =
        client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    let plan_family_b =
        client.create_plan_template(&merchant, &(AMOUNT * 2), &INTERVAL, &false, &None::<i128>);

    let sub_id = client.create_subscription_from_plan(&subscriber, &plan_family_a);

    let result = client.try_migrate_subscription_to_plan(&subscriber, &sub_id, &plan_family_b);
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn test_migrate_subscription_requires_plan_origin() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create a subscription directly (not from a plan template).
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    // Create a plan template to migrate to.
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    let result = client.try_migrate_subscription_to_plan(&subscriber, &sub_id, &plan_id);
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn test_update_plan_template_cannot_change_token() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let other_token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);
    client.add_accepted_token(&admin, &other_token, &6);

    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    let original = client.get_plan_template(&plan_id);

    // Even if we conceptually want to move to a different token, versioning API
    // does not allow this; such a change should use a separate template family.
    let _ = original; // silence unused, documentation-only test narrative.

    // We indirectly assert this by verifying that update_plan_template always
    // keeps the existing token.
    let new_plan_id = client.update_plan_template(
        &merchant,
        &plan_id,
        &(AMOUNT * 2),
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let updated = client.get_plan_template(&new_plan_id);
    assert_eq!(updated.token, token);
}

/// Subscriber can withdraw remaining prepaid balance after cap-triggered cancellation.
#[test]
fn test_cap_cancelled_subscriber_can_withdraw() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let subscriber = Address::generate(&env);
    let token = create_token_and_mint(&env, &subscriber, 1_000_000_000i128);
    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.init(&token, &6, &admin, &1_000_000i128, &0u64);

    let cap = 2 * AMOUNT;
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(cap),
    );

    // Fund the subscription so the vault holds real tokens for withdrawal.
    client.deposit_funds(&sub_id, &subscriber, &PREPAID);

    // Advance time and charge twice — second charge hits cap → Cancelled
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&sub_id);
    env.ledger()
        .with_mut(|li| li.timestamp = T0 + 2 * INTERVAL + 1);
    client.charge_subscription(&sub_id);

    let sub_after = client.get_subscription(&sub_id);
    assert_eq!(sub_after.status, SubscriptionStatus::Cancelled);
    assert!(sub_after.prepaid_balance > 0);

    // Subscriber can withdraw remaining prepaid balance
    client.withdraw_subscriber_funds(&sub_id, &subscriber);
    let sub_final = client.get_subscription(&sub_id);
    assert_eq!(sub_final.prepaid_balance, 0);
}

#[test]
fn test_charge_usage_basic() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &true,
        &None::<i128>,
    );
    seed_balance(&env, &client, id, PREPAID);

    client.charge_usage(&id, &1_000_000);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, PREPAID - 1_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #1004)")]
fn test_charge_usage_not_enabled() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);
    client.charge_usage(&id, &1_000_000);
}

// -- Merchant tests -----------------------------------------------------------

#[test]
fn test_merchant_balance_and_withdrawal() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id, _, merchant) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);

    let balance = client.get_merchant_balance(&merchant);
    assert!(balance > 0);
}

// -- List subscriptions by subscriber test ------------------------------------

#[test]
fn test_list_subscriptions_by_subscriber() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let id1 = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let id2 = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    let page = client.list_subscriptions_by_subscriber(&subscriber, &0, &10);
    assert_eq!(page.subscription_ids.len(), 2);
    assert_eq!(page.subscription_ids.get(0).unwrap(), id1);
    assert_eq!(page.subscription_ids.get(1).unwrap(), id2);
    assert!(!page.has_next);
}

// -- Subscriber withdrawal test -----------------------------------------------

#[test]
fn test_withdraw_subscriber_funds_after_cancel() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&subscriber, &100_000_000);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &5_000_000);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 5_000_000);

    client.cancel_subscription(&id, &subscriber);
    client.withdraw_subscriber_funds(&id, &subscriber);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 0);
}

// -- Export tests -------------------------------------------------------------

#[test]
fn test_export_contract_snapshot() {
    let (env, client, _, admin) = setup_test_env();
    let snapshot = client.export_contract_snapshot(&admin);
    assert_eq!(snapshot.admin, admin);
    assert_eq!(snapshot.storage_version, 2);
}

#[test]
fn test_export_subscription_summaries() {
    let (env, client, _, admin) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let summaries = client.export_subscription_summaries(&admin, &0, &10);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries.get(0).unwrap().subscription_id, id);
}

// =============================================================================
// Metadata Key-Value Store Tests
// =============================================================================

#[test]
fn test_metadata_set_and_get() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "invoice_id");
    let value = String::from_str(&env, "INV-2025-001");

    client.set_metadata(&id, &subscriber, &key, &value);

    let result = client.get_metadata(&id, &key);
    assert_eq!(result, value);
}

#[test]
fn test_metadata_update_existing_key() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "customer_id");
    let value1 = String::from_str(&env, "CUST-001");
    let value2 = String::from_str(&env, "CUST-002");

    client.set_metadata(&id, &subscriber, &key, &value1);
    assert_eq!(client.get_metadata(&id, &key), value1);

    client.set_metadata(&id, &subscriber, &key, &value2);
    assert_eq!(client.get_metadata(&id, &key), value2);

    // Key count should still be 1 (updated, not duplicated)
    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 1);
}

#[test]
fn test_metadata_delete() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    let value = String::from_str(&env, "premium");

    client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(client.get_metadata(&id, &key), value);

    client.delete_metadata(&id, &subscriber, &key);

    let result = client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
fn test_metadata_list_keys() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key1 = String::from_str(&env, "invoice_id");
    let key2 = String::from_str(&env, "customer_id");
    let key3 = String::from_str(&env, "campaign_tag");

    client.set_metadata(&id, &subscriber, &key1, &String::from_str(&env, "v1"));
    client.set_metadata(&id, &subscriber, &key2, &String::from_str(&env, "v2"));
    client.set_metadata(&id, &subscriber, &key3, &String::from_str(&env, "v3"));

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 3);
}

#[test]
fn test_metadata_empty_list_for_new_subscription() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 0);
}

#[test]
fn test_metadata_merchant_can_set() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, merchant) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "merchant_ref");
    let value = String::from_str(&env, "MR-123");

    client.set_metadata(&id, &merchant, &key, &value);
    assert_eq!(client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_merchant_can_delete() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, merchant) =
        create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    let value = String::from_str(&env, "test");

    // Subscriber sets it
    client.set_metadata(&id, &subscriber, &key, &value);

    // Merchant deletes it
    client.delete_metadata(&id, &merchant, &key);

    let result = client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_metadata_unauthorized_actor_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let stranger = Address::generate(&env);
    let key = String::from_str(&env, "test");
    let value = String::from_str(&env, "val");

    client.set_metadata(&id, &stranger, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_metadata_delete_unauthorized_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "test");
    client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "val"));

    let stranger = Address::generate(&env);
    client.delete_metadata(&id, &stranger, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #1023)")]
fn test_metadata_key_limit_enforced() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Set MAX_METADATA_KEYS (10) keys
    for i in 0..10u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        let value = String::from_str(&env, "val");
        client.set_metadata(&id, &subscriber, &key, &value);
    }

    // 11th key should fail
    let key = String::from_str(&env, "key_overflow");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
fn test_metadata_update_at_limit_succeeds() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Fill to max
    for i in 0..10u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "val"));
    }

    // Updating an existing key should succeed even at limit
    let key = String::from_str(&env, "key_0");
    let new_value = String::from_str(&env, "updated");
    client.set_metadata(&id, &subscriber, &key, &new_value);
    assert_eq!(client.get_metadata(&id, &key), new_value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1024)")]
fn test_metadata_key_too_long_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // 33 chars exceeds MAX_METADATA_KEY_LENGTH (32)
    let key = String::from_str(&env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1024)")]
fn test_metadata_empty_key_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1025)")]
fn test_metadata_value_too_long_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "test");
    // Create a string > 256 bytes
    let long_str = alloc::string::String::from_utf8(alloc::vec![b'x'; 257]).unwrap();
    let long_value = String::from_str(&env, &long_str);
    client.set_metadata(&id, &subscriber, &key, &long_value);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_get_nonexistent_key() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "nonexistent");
    client.get_metadata(&id, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_delete_nonexistent_key() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "nonexistent");
    client.delete_metadata(&id, &subscriber, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_operations_on_nonexistent_subscription() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let key = String::from_str(&env, "test");
    let value = String::from_str(&env, "val");
    client.set_metadata(&999, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1002)")]
fn test_metadata_set_on_cancelled_subscription_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);

    let key = String::from_str(&env, "test");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
fn test_metadata_does_not_affect_financial_state() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    let sub_before = client.get_subscription(&id);

    // Set multiple metadata entries
    for i in 0..5u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        let value = String::from_str(&env, &format!("value_{i}"));
        client.set_metadata(&id, &subscriber, &key, &value);
    }

    let sub_after = client.get_subscription(&id);

    // Financial state must be unchanged
    assert_eq!(sub_before.prepaid_balance, sub_after.prepaid_balance);
    assert_eq!(sub_before.lifetime_charged, sub_after.lifetime_charged);
    assert_eq!(sub_before.status, sub_after.status);
    assert_eq!(sub_before.amount, sub_after.amount);
}

#[test]
fn test_metadata_delete_then_re_add() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    let value1 = String::from_str(&env, "v1");
    let value2 = String::from_str(&env, "v2");

    client.set_metadata(&id, &subscriber, &key, &value1);
    client.delete_metadata(&id, &subscriber, &key);

    // Re-add same key with different value
    client.set_metadata(&id, &subscriber, &key, &value2);
    assert_eq!(client.get_metadata(&id, &key), value2);

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 1);
}

#[test]
fn test_metadata_delete_frees_key_slot() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Fill to max
    for i in 0..10u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "v"));
    }

    // Delete one
    client.delete_metadata(&id, &subscriber, &String::from_str(&env, "key_5"));

    // Should now be able to add a new key
    let new_key = String::from_str(&env, "key_new");
    client.set_metadata(&id, &subscriber, &new_key, &String::from_str(&env, "v"));

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 10);
}

#[test]
fn test_metadata_isolation_between_subscriptions() {
    let (env, client, _, _) = setup_test_env();
    let (id1, sub1, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let (id2, sub2, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "invoice_id");
    let val1 = String::from_str(&env, "INV-001");
    let val2 = String::from_str(&env, "INV-002");

    client.set_metadata(&id1, &sub1, &key, &val1);
    client.set_metadata(&id2, &sub2, &key, &val2);

    assert_eq!(client.get_metadata(&id1, &key), val1);
    assert_eq!(client.get_metadata(&id2, &key), val2);
}

#[test]
fn test_metadata_on_paused_subscription_allowed() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);

    let key = String::from_str(&env, "note");
    let value = String::from_str(&env, "paused for maintenance");
    client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_delete_on_cancelled_subscription_allowed() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "v"));

    client.cancel_subscription(&id, &subscriber);

    // Delete should still work on cancelled (cleanup)
    client.delete_metadata(&id, &subscriber, &key);
    let result = client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
fn test_billing_statements_offset_pagination_newest_first() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);
    let (client, token, _admin) = setup_contract(&env);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&subscriber, &1_000_000_000i128);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &true,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &200_000_000i128);

    for i in 1..=6 {
        env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        client.charge_subscription(&id);
    }

    let page1 = client.get_sub_statements_offset(&id, &0, &2, &true);
    assert_eq!(page1.total, 6);
    assert_eq!(page1.statements.len(), 2);
    assert_eq!(page1.statements.get(0).unwrap().sequence, 5);
    assert_eq!(page1.statements.get(1).unwrap().sequence, 4);

    let page2 = client.get_sub_statements_offset(&id, &2, &2, &true);
    assert_eq!(page2.statements.len(), 2);
    assert_eq!(page2.statements.get(0).unwrap().sequence, 3);
    assert_eq!(page2.statements.get(1).unwrap().sequence, 2);
}

#[test]
fn test_billing_statements_cursor_pagination_boundaries() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);
    let (client, token, _admin) = setup_contract(&env);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&subscriber, &1_000_000_000i128);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &true,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &200_000_000i128);

    for i in 1..=4 {
        env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        client.charge_subscription(&id);
    }

    let first = client.get_sub_statements_cursor(&id, &None::<u32>, &3, &true);
    assert_eq!(first.statements.len(), 3);
    assert_eq!(first.statements.get(0).unwrap().sequence, 3);
    assert_eq!(first.statements.get(2).unwrap().sequence, 1);
    assert_eq!(first.next_cursor, Some(0));

    let second = client.get_sub_statements_cursor(&id, &first.next_cursor, &3, &true);
    assert_eq!(second.statements.len(), 1);
    assert_eq!(second.statements.get(0).unwrap().sequence, 0);
    assert_eq!(second.next_cursor, None);

    let invalid_cursor = client.get_sub_statements_cursor(&id, &Some(99u32), &2, &true);
    assert_eq!(invalid_cursor.statements.len(), 0);
    assert_eq!(invalid_cursor.next_cursor, None);
    assert_eq!(invalid_cursor.total, 4);
}

#[test]
fn test_compaction_prunes_old_statements_and_keeps_recent() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);
    let (client, token, admin) = setup_contract(&env);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&subscriber, &2_000_000_000i128);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &500_000_000i128);

    for i in 1..=8 {
        env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        client.charge_subscription(&id);
    }

    client.set_billing_retention(&admin, &3);
    let summary = client.compact_billing_statements(&admin, &id, &None::<u32>);
    assert_eq!(summary.pruned_count, 5);
    assert_eq!(summary.kept_count, 3);
    assert_eq!(summary.total_pruned_amount, 5_000_000i128);

    let page = client.get_sub_statements_offset(&id, &0, &10, &true);
    assert_eq!(page.total, 3);
    assert_eq!(page.statements.len(), 3);
    assert_eq!(page.statements.get(0).unwrap().sequence, 7);
    assert_eq!(page.statements.get(2).unwrap().sequence, 5);

    let agg = client.get_stmt_compacted_aggregate(&id);
    assert_eq!(agg.pruned_count, 5);
    assert_eq!(agg.total_amount, 5_000_000i128);
}

#[test]
fn test_compaction_no_rows_and_override_value() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _token, admin) = setup_contract(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    let summary = client.compact_billing_statements(&admin, &id, &Some(10u32));
    assert_eq!(summary.pruned_count, 0);
    assert_eq!(summary.kept_count, 0);
    assert_eq!(summary.total_pruned_amount, 0);
}

#[test]
fn test_oracle_enabled_charge_uses_quote_conversion() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);
    let (client, token, admin) = setup_contract(&env);
    let oracle_id = env.register(MockOracle, ());
    let oracle = MockOracleClient::new(&env, &oracle_id);
    oracle.set_price(&2_000_000i128, &T0); // 2 quote units/token with 6 decimals

    // Enable oracle pricing with non-stale quote.
    client.set_oracle_config(
        &admin,
        &true,
        &Some(oracle_id.clone()),
        &(60 * 24 * 60 * 60),
    );

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&subscriber, &2_000_000_000i128);

    // 20 quote units (6 decimals). At price 2 quote/token, charge should be 10 tokens.
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &20_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &100_000_000i128);

    env.ledger().set_timestamp(T0 + INTERVAL);
    client.charge_subscription(&id);

    assert_eq!(client.get_merchant_balance(&merchant), 10_000_000i128);
}

#[test]
fn test_oracle_stale_quote_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0 + INTERVAL);
    let (client, token, admin) = setup_contract(&env);
    let oracle_id = env.register(MockOracle, ());
    let oracle = MockOracleClient::new(&env, &oracle_id);
    oracle.set_price(&2_000_000i128, &T0); // stale vs max_age=1
    client.set_oracle_config(&admin, &true, &Some(oracle_id.clone()), &1u64);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&subscriber, &2_000_000_000i128);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &20_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &100_000_000i128);

    let result = client.try_charge_subscription(&id);
    assert_eq!(result, Err(Ok(Error::OraclePriceStale)));
}

#[test]
fn test_multi_token_balances_are_isolated_per_token() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(T0);
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    client.init(&token_a, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
    client.add_accepted_token(&admin, &token_b, &6);

    let merchant = Address::generate(&env);
    let subscriber_a = Address::generate(&env);
    let subscriber_b = Address::generate(&env);
    soroban_sdk::token::StellarAssetClient::new(&env, &token_a)
        .mint(&subscriber_a, &100_000_000i128);
    soroban_sdk::token::StellarAssetClient::new(&env, &token_b)
        .mint(&subscriber_b, &100_000_000i128);

    let id_a = client.create_subscription(
        &subscriber_a,
        &merchant,
        &5_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let id_b = client.create_subscription_with_token(
        &subscriber_b,
        &merchant,
        &token_b,
        &7_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id_a, &subscriber_a, &20_000_000i128);
    client.deposit_funds(&id_b, &subscriber_b, &20_000_000i128);

    env.ledger().set_timestamp(T0 + INTERVAL);
    client.charge_subscription(&id_a);
    client.charge_subscription(&id_b);

    assert_eq!(
        client.get_merchant_balance_by_token(&merchant, &token_a),
        5_000_000i128
    );
    assert_eq!(
        client.get_merchant_balance_by_token(&merchant, &token_b),
        7_000_000i128
    );
}

#[test]
fn test_create_subscription_with_unaccepted_token_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _token, _admin) = setup_contract(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let unsupported = Address::generate(&env);
    let result = client.try_create_subscription_with_token(
        &subscriber,
        &merchant,
        &unsupported,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

// Added pagination correctness tests for billing statements

#[test]
fn test_offset_pagination_ordering_newest_first() {
    let (env, client, _token, _admin) = setup_test_env();
    let sub_id = 1u32;

    // append 5 statements
    for i in 0..5u32 {
        crate::statements::append_statement(
            &env,
            sub_id,
            1000 + i as i128,
            Address::generate(&env),
            crate::types::BillingChargeKind::Charge,
            i as u64,
            i as u64 + 10,
        );
    }

    let page = crate::statements::get_statements_by_subscription_offset(
        &env,
        sub_id,
        0,
        5,
        true,
    )
    .unwrap();

    assert_eq!(page.statements.len(), 5);
    // newest first => last appended first
    assert!(page.statements.get(0).unwrap().amount > page.statements.get(4).unwrap().amount);
}

#[test]
fn test_offset_pagination_ordering_oldest_first() {
    let (env, _client, _token, _admin) = setup_test_env();
    let sub_id = 2u32;

    for i in 0..5u32 {
        crate::statements::append_statement(
            &env,
            sub_id,
            1000 + i as i128,
            Address::generate(&env),
            crate::types::BillingChargeKind::Charge,
            i as u64,
            i as u64 + 10,
        );
    }

    let page = crate::statements::get_statements_by_subscription_offset(
        &env,
        sub_id,
        0,
        5,
        false,
    )
    .unwrap();

    assert!(page.statements.get(0).unwrap().amount < page.statements.get(4).unwrap().amount);
}

#[test]
fn test_cursor_pagination_continuity() {
    let (env, _client, _token, _admin) = setup_test_env();
    let sub_id = 3u32;

    for i in 0..10u32 {
        crate::statements::append_statement(
            &env,
            sub_id,
            1000 + i as i128,
            Address::generate(&env),
            crate::types::BillingChargeKind::Charge,
            i as u64,
            i as u64 + 10,
        );
    }

    let first = crate::statements::get_statements_by_subscription_cursor(
        &env,
        sub_id,
        None,
        4,
        true,
    )
    .unwrap();

    assert_eq!(first.statements.len(), 4);
    assert!(first.next_cursor.is_some());

    let second = crate::statements::get_statements_by_subscription_cursor(
        &env,
        sub_id,
        first.next_cursor,
        4,
        true,
    )
    .unwrap();

    assert_eq!(second.statements.len(), 4);
}

#[test]
fn test_cursor_termination() {
    let (env, _client, _token, _admin) = setup_test_env();
    let sub_id = 4u32;

    for i in 0..3u32 {
        crate::statements::append_statement(
            &env,
            sub_id,
            1000 + i as i128,
            Address::generate(&env),
            crate::types::BillingChargeKind::Charge,
            i as u64,
            i as u64 + 10,
        );
    }

    let mut cursor = None;
    let mut total_fetched = 0;

    loop {
        let page = crate::statements::get_statements_by_subscription_cursor(
            &env,
            sub_id,
            cursor,
            2,
            true,
        )
        .unwrap();

        total_fetched += page.statements.len();
        if page.next_cursor.is_none() {
            break;
        }
        cursor = page.next_cursor;
    }

    assert_eq!(total_fetched, 3);
}

#[test]
fn test_invalid_limit() {
    let (env, _client, _token, _admin) = setup_test_env();
    let sub_id = 5u32;

    let result = crate::statements::get_statements_by_subscription_cursor(
        &env,
        sub_id,
        None,
        0,
        true,
    );

    assert!(result.is_err());
}

#[test]
fn test_empty_history() {
    let (env, _client, _token, _admin) = setup_test_env();

    let page = crate::statements::get_statements_by_subscription_cursor(
        &env,
        999,
        None,
        5,
        true,
    )
    .unwrap();

    assert_eq!(page.statements.len(), 0);
    assert!(page.next_cursor.is_none());
}
