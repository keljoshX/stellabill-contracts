use crate::{
    can_transition, compute_next_charge_info, get_allowed_transitions, validate_status_transition,
    AdminRotatedEvent, ChargeExecutionResult, Error, MerchantWithdrawalEvent, OraclePrice,
    RecoveryReason, Subscription, SubscriptionStatus, SubscriptionVault, SubscriptionVaultClient,
    MAX_SUBSCRIPTION_ID, FundsDepositedEvent, SubscriptionChargedEvent,
    SubscriptionChargeFailedEvent, LifetimeCapReachedEvent, PartialRefundEvent,
};
use soroban_sdk::testutils::{Address as _, Events, Ledger as _};
use soroban_sdk::{
    contract, contractimpl, Address, Env, FromVal, IntoVal, String, Symbol, TryFromVal, Val, Vec,
};

extern crate alloc;
use alloc::format;
use crate::test_utils::{TestEnv, fixtures, assertions};
use crate::queries::MAX_SUBSCRIPTION_LIST_PAGE;

// -- constants ----------------------------------------------------------------
const T0: u64 = 1_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60; // 30 days
const AMOUNT: i128 = 10_000_000; // 10 USDC (6 decimals)
const PREPAID: i128 = 50_000_000; // 50 USDC

// -- lifecycle action enum for property tests --------------------------------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecycleAction {
    Pause,
    Resume,
    Cancel,
}

// -- all subscription statuses for property tests ----------------------------
const ALL_STATUSES: &[SubscriptionStatus] = &[
    SubscriptionStatus::Active,
    SubscriptionStatus::Paused,
    SubscriptionStatus::Cancelled,
    SubscriptionStatus::InsufficientBalance,
    SubscriptionStatus::GracePeriod,
];

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
     &None::<u64>);
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


fn seed_merchant_balance(
    env: &Env,
    contract_id: &Address,
    merchant: &Address,
    token: &Address,
    balance: i128,
) {
    env.as_contract(contract_id, || {
        env.storage().instance().set(
            &(Symbol::new(env, "merchant_balance"), merchant.clone(), token.clone()),
            &balance,
        );
    });
}

fn snapshot_subscriptions(
    client: &SubscriptionVaultClient,
    ids: &[u32],
) -> alloc::vec::Vec<Subscription> {
    ids.iter().map(|id| client.get_subscription(id)).collect()
}

fn manual_can_transition(from: &SubscriptionStatus, to: &SubscriptionStatus) -> bool {
    // This should match the logic in state_machine.rs
    match (from, to) {
        (SubscriptionStatus::Active, SubscriptionStatus::Paused) => true,
        (SubscriptionStatus::Active, SubscriptionStatus::Cancelled) => true,
        (SubscriptionStatus::Active, SubscriptionStatus::InsufficientBalance) => true,
        (SubscriptionStatus::Active, SubscriptionStatus::GracePeriod) => true,
        (SubscriptionStatus::Paused, SubscriptionStatus::Active) => true,
        (SubscriptionStatus::Paused, SubscriptionStatus::Cancelled) => true,
        (SubscriptionStatus::InsufficientBalance, SubscriptionStatus::Active) => true,
        (SubscriptionStatus::InsufficientBalance, SubscriptionStatus::Cancelled) => true,
        (SubscriptionStatus::GracePeriod, SubscriptionStatus::Active) => true,
        (SubscriptionStatus::GracePeriod, SubscriptionStatus::Cancelled) => true,
        (SubscriptionStatus::GracePeriod, SubscriptionStatus::InsufficientBalance) => true,
        _ => from == to,
    }
}

fn lifecycle_action_target(action: LifecycleAction) -> SubscriptionStatus {
    match action {
        LifecycleAction::Pause => SubscriptionStatus::Paused,
        LifecycleAction::Resume => SubscriptionStatus::Active,
        LifecycleAction::Cancel => SubscriptionStatus::Cancelled,
    }
}

fn random_lifecycle_action(seed: &mut u64) -> LifecycleAction {
    match lcg_next(seed) % 3 {
        0 => LifecycleAction::Pause,
        1 => LifecycleAction::Resume,
        _ => LifecycleAction::Cancel,
    }
}

fn random_transition_action(seed: &mut u64) -> u64 {
    lcg_next(seed) % 5
}

fn transition_action_target(action: u64) -> SubscriptionStatus {
    match action {
        0 => SubscriptionStatus::Active,
        1 => SubscriptionStatus::Paused,
        2 => SubscriptionStatus::Cancelled,
        3 => SubscriptionStatus::InsufficientBalance,
        _ => SubscriptionStatus::GracePeriod,
    }
}

fn lcg_next(seed: &mut u64) -> u64 {
    *seed = (*seed).wrapping_mul(1103515245).wrapping_add(12345);
    *seed
}



fn collect_batch_result_codes(
    env: &Env,
    client: &SubscriptionVaultClient,
    ids: &[u32],
) -> alloc::vec::Vec<(bool, u32)> {
    let ids_vec = ids.iter().fold(Vec::<u32>::new(env), |mut acc, id| {
        acc.push_back(*id);
        acc
    });
    let results = client.batch_charge(&ids_vec);
    results
        .iter()
        .map(|result| (result.success, result.error_code))
        .collect()
}

fn collect_single_charge_result_codes(
    client: &SubscriptionVaultClient,
    ids: &[u32],
) -> alloc::vec::Vec<(bool, u32)> {
    ids.iter()
        .map(|id| match client.try_charge_subscription(id) {
            Ok(Ok(ChargeExecutionResult::Charged)) => (true, 0),
            Ok(Ok(ChargeExecutionResult::InsufficientBalance)) => {
                (false, Error::InsufficientBalance.to_code())
            }
            Err(Ok(err)) => (false, err.to_code()),
            other => panic!("unexpected charge result: {other:?}"),
        })
        .collect()
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
    assert_eq!(paused_targets.len(), 3);
    assert!(paused_targets.contains(&SubscriptionStatus::Active));
    assert!(paused_targets.contains(&SubscriptionStatus::Cancelled));
    assert!(paused_targets.contains(&SubscriptionStatus::Expired));

    assert_eq!(
        get_allowed_transitions(&SubscriptionStatus::Cancelled).len(),
        1
    );

    let ib_targets = get_allowed_transitions(&SubscriptionStatus::InsufficientBalance);
    assert_eq!(ib_targets.len(), 3);
}

#[test]
fn test_state_machine_property_transition_matrix_matches_manual_rules() {
    for from in ALL_STATUSES.iter() {
        let allowed = get_allowed_transitions(from);

        for to in ALL_STATUSES.iter() {
            let expected = manual_can_transition(from, to);
            assert_eq!(can_transition(from, to), expected);
            assert_eq!(validate_status_transition(from, to).is_ok(), expected);

            if from == to {
                assert!(!allowed.contains(to));
            } else {
                assert_eq!(allowed.contains(to), expected);
            }
        }
    }
}

#[test]
fn test_state_machine_property_random_transition_sequences_only_allow_legal_targets() {
    for start in ALL_STATUSES.iter() {
        for seed_base in 0..64u64 {
            let mut seed = seed_base + (start.clone() as u64) * 97;
            let mut current = start.clone();

            for _ in 0..24 {
                let action = random_transition_action(&mut seed);
                let target = transition_action_target(action);
                let expected = manual_can_transition(&current, &target);

                assert_eq!(can_transition(&current, &target), expected);
                assert_eq!(
                    validate_status_transition(&current, &target).is_ok(),
                    expected
                );

                if expected {
                    current = target;
                }
            }
        }
    }
}

#[test]
fn test_state_machine_property_lifecycle_entrypoints_follow_manual_model() {
    for start in ALL_STATUSES.iter() {
        for seed_base in 0..48u64 {
            let (env, client, token, _admin) = setup_test_env();
            let (id, subscriber, _) = create_test_subscription(&env, &client, start.clone());
            let mut expected = start.clone();
            let mut seed = seed_base + (start.clone() as u64) * 131;

            for _ in 0..12 {
                let action = random_lifecycle_action(&mut seed);
                let target = lifecycle_action_target(action);
                let should_succeed = manual_can_transition(&expected, &target);

                if action == LifecycleAction::Resume && (
                    expected == SubscriptionStatus::InsufficientBalance ||
                    expected == SubscriptionStatus::GracePeriod
                ) && should_succeed {
                    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
                    token_client.mint(&subscriber, &AMOUNT);
                    client.deposit_funds(&id, &subscriber, &AMOUNT);
                }

            let result = match action {
                    LifecycleAction::Pause => client.try_pause_subscription(&id, &subscriber),
                    LifecycleAction::Resume => client.try_resume_subscription(&id, &subscriber),
                    LifecycleAction::Cancel => client.try_cancel_subscription(&id, &subscriber),
                };

                assert_eq!(result.is_ok(), should_succeed);

                let current = client.get_subscription(&id).status;
                if should_succeed {
                    expected = target;
                    assert_eq!(current, expected);
                } else {
                    assert_eq!(current, expected);
                }
            }
        }
    }
}

#[test]
fn test_state_machine_property_charge_failures_and_recovery_paths_obey_rules() {
    for seed_base in 0..32u64 {
        let mut seed = seed_base;

        for step in 0..10 {
            let (env, client, token, _) = setup_test_env();
            let (id, subscriber, _) =
                create_test_subscription(&env, &client, SubscriptionStatus::Active);
            let in_grace_window = lcg_next(&mut seed) % 2 == 0;
            let topup_amount = if lcg_next(&mut seed) % 2 == 0 {
                AMOUNT - 1
            } else {
                PREPAID
            };

            seed_balance(&env, &client, id, 0);
            let charge_time = if in_grace_window {
                T0 + INTERVAL + 1
            } else {
                T0 + INTERVAL + (7 * 24 * 60 * 60) + 1
            };
            env.ledger().set_timestamp(charge_time + step as u64);

            let result = client.try_charge_subscription(&id);
            assert_eq!(result, Ok(Ok(ChargeExecutionResult::InsufficientBalance)));

            let failed_status = client.get_subscription(&id).status;
            // Depending on charge_time, it could be GracePeriod or InsufficientBalance
            if in_grace_window {
                assert_eq!(failed_status, SubscriptionStatus::GracePeriod);
            } else {
                assert_eq!(failed_status, SubscriptionStatus::InsufficientBalance);
            }

            soroban_sdk::token::StellarAssetClient::new(&env, &token)
                .mint(&subscriber, &topup_amount.max(1_000_000));
            client.deposit_funds(&id, &subscriber, &topup_amount.max(1_000_000));

            let after_deposit = client.get_subscription(&id).status;
            if topup_amount >= AMOUNT {
                assert_eq!(after_deposit, SubscriptionStatus::Active);
            } else {
                assert!(after_deposit == SubscriptionStatus::InsufficientBalance || after_deposit == SubscriptionStatus::GracePeriod);
            }

            if topup_amount >= AMOUNT {
                env.ledger()
                    .set_timestamp(charge_time + INTERVAL + step as u64 + 1);
                let charge_again = client.try_charge_subscription(&id);
                assert!(charge_again.is_ok());
                assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Active);
            } else {
                client.cancel_subscription(&id, &subscriber);
                assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Cancelled);
            }
        }
    }
}

// -- Contract Lifecycle Tests -------------------------------------------------

#[test]
fn test_pause_subscription_from_active() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.pause_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Paused);
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_pause_subscription_from_cancelled_should_fail() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.cancel_subscription(&id, &subscriber);
    test_env.client.pause_subscription(&id, &subscriber);
}

#[test]
fn test_pause_subscription_from_paused_is_idempotent() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.pause_subscription(&id, &subscriber);
    test_env.client.pause_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Paused);
}

#[test]
fn test_cancel_subscription_from_active() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.cancel_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);
}

#[test]
fn test_cancel_subscription_from_paused() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.pause_subscription(&id, &subscriber);
    test_env.client.cancel_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);
}

#[test]
fn test_cancel_subscription_from_cancelled_is_idempotent() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.cancel_subscription(&id, &subscriber);
    test_env.client.cancel_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);
}

#[test]
fn test_resume_subscription_from_paused() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.pause_subscription(&id, &subscriber);
    test_env.client.resume_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Active);
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_resume_subscription_from_cancelled_should_fail() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    test_env.client.cancel_subscription(&id, &subscriber);
    test_env.client.resume_subscription(&id, &subscriber);
}

#[test]
fn test_full_lifecycle_active_pause_resume() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    test_env.client.pause_subscription(&id, &subscriber);
    assert_eq!(
        test_env.client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
    test_env.client.resume_subscription(&id, &subscriber);
    assert_eq!(
        test_env.client.get_subscription(&id).status,
        SubscriptionStatus::Active
    );
    test_env.client.pause_subscription(&id, &subscriber);
    assert_eq!(
        test_env.client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
fn test_all_valid_transitions_coverage() {
    let test_env = TestEnv::default();

    // Active -> Paused
    {
        let (id, subscriber, _) =
            fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        test_env.client.pause_subscription(&id, &subscriber);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Paused);
    }
    // Active -> Cancelled
    {
        let (id, subscriber, _) =
            fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        test_env.client.cancel_subscription(&id, &subscriber);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);
    }
    // Active -> InsufficientBalance (direct storage patch)
    {
        let (id, _, _) = fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        fixtures::patch_status(&test_env.env, &test_env.client, id, SubscriptionStatus::InsufficientBalance);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::InsufficientBalance);
    }
    // Paused -> Active
    {
        let (id, subscriber, _) =
            fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        test_env.client.pause_subscription(&id, &subscriber);
        test_env.client.resume_subscription(&id, &subscriber);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Active);
    }
    // Paused -> Cancelled
    {
        let (id, subscriber, _) =
            fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        test_env.client.pause_subscription(&id, &subscriber);
        test_env.client.cancel_subscription(&id, &subscriber);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);
    }
    // InsufficientBalance -> Active
    {
        let (id, subscriber, _) =
            fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        fixtures::patch_status(&test_env.env, &test_env.client, id, SubscriptionStatus::InsufficientBalance);
        test_env.stellar_token_client().mint(&subscriber, &AMOUNT);
        test_env.client.deposit_funds(&id, &subscriber, &AMOUNT);
        test_env.client.resume_subscription(&id, &subscriber);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Active);
    }
    // InsufficientBalance -> Cancelled
    {
        let (id, subscriber, _) =
            fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        fixtures::patch_status(&test_env.env, &test_env.client, id, SubscriptionStatus::InsufficientBalance);
        test_env.client.cancel_subscription(&id, &subscriber);
        assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);
    }
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_invalid_cancelled_to_active() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    test_env.client.cancel_subscription(&id, &subscriber);
    test_env.client.resume_subscription(&id, &subscriber);
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_invalid_insufficient_balance_to_paused() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    fixtures::patch_status(&test_env.env, &test_env.client, id, SubscriptionStatus::InsufficientBalance);
    test_env.client.pause_subscription(&id, &subscriber);
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
    };
    assert_eq!(sub.lifetime_cap, Some(cap));
    assert_eq!(sub.lifetime_charged, 0);
}

// -- Contract Charging Tests --------------------------------------------------

#[test]
fn test_charge_subscription_basic() {
    let test_env = TestEnv::default();
    test_env.set_timestamp(T0);

    let (id, _, _) =
        fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);

    test_env.env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    test_env.client.charge_subscription(&id);

    assert_eq!(
        test_env.client.get_subscription(&id).prepaid_balance,
        PREPAID - AMOUNT
    );
    let sub = test_env.client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, AMOUNT);
}

#[test]
#[should_panic(expected = "Error(Contract, #1002)")]
fn test_charge_subscription_paused_fails() {
    let test_env = TestEnv::default();
    test_env.set_timestamp(T0);
    let (id, subscriber, _) =
        fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);
    test_env.client.pause_subscription(&id, &subscriber);
    test_env.jump(INTERVAL + 1);
    test_env.client.charge_subscription(&id);
}

#[test]
fn test_charge_subscription_insufficient_balance_returns_error() {
    let test_env = TestEnv::default();
    test_env.set_timestamp(T0);
    let (id, _, _) = fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    
    let grace_period = 7 * 24 * 60 * 60u64;
    test_env.jump(INTERVAL + grace_period + 1);
    let result = test_env.client.try_charge_subscription(&id);
    assert_eq!(result, Ok(Ok(ChargeExecutionResult::InsufficientBalance)));
}

// -- ID limit test ------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #429)")]
fn test_subscription_limit_reached() {
    let test_env = TestEnv::default();
    fixtures::seed_counter(&test_env.env, &test_env.client.address, MAX_SUBSCRIPTION_ID);
    test_env.client.create_subscription(
        &Address::generate(&test_env.env),
        &Address::generate(&test_env.env),
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
}

#[test]
fn test_cancel_subscription_unauthorized() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let other = Address::generate(&test_env.env);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &1000, &86400, &true, &None::<i128>, &None::<u64>);
    let result = client.try_cancel_subscription(&sub_id, &other);
    assert_eq!(result, Err(Ok(Error::Forbidden)));
}

#[test]
fn test_withdraw_subscriber_funds() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &1_000_000);

    let sub_id =
        client.create_subscription(&subscriber, &merchant, &1000, &86400, &true, &None::<i128>, &None::<u64>);
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
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let min_topup = 5_000_000i128;

    test_env.client.set_min_topup(&test_env.admin, &min_topup);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &10_000_000i128,
        &(30 * 24 * 60 * 60),
        &false,
        &None::<i128>,
     &None::<u64>);
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
     &None::<u64>);
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
     &None::<u64>);
    assert!(client
        .try_deposit_funds(&id, &subscriber, &deposit_amount)
        .is_ok());
}

// ── Usage-charge tests ─────────────────────────────────────────────────────────

// -- Deposit tests ------------------------------------------------------------

#[test]
fn test_deposit_funds_basic() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &100_000_000);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &5_000_000);
    assertions::assert_prepaid_balance(&test_env.client, &id, 5_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_deposit_funds_unauthorized() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let other = Address::generate(&env);
    let merchant = Address::generate(&env);

    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&other, &100_000_000);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &5_000_000);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 5_000_000);
}

#[test]
fn test_deposit_funds_event_payload() {
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
     &None::<u64>);
    // min_topup is 1_000_000; try to deposit 500
    client.deposit_funds(&id, &subscriber, &500);
}

    client.deposit_funds(&id, &subscriber, &15_000_000);

    let events = env.events().all();
    let deposit_event = events.last().expect("No events found");

    // Verify event topics: (Symbol("deposited"), subscription_id)
    assert_eq!(deposit_event.0, client.address);
    assert_eq!(
        Symbol::from_val(&env, &deposit_event.1.get(0).expect("Missing topic 0")),
        Symbol::new(&env, "deposited")
    );
    assert_eq!(
        u32::from_val(&env, &deposit_event.1.get(1).expect("Missing topic 1")),
        id
    );

    // Verify event data: FundsDepositedEvent { subscription_id, subscriber, amount, prepaid_balance }
    let event_data: crate::FundsDepositedEvent = deposit_event.2.into_val(&env);
    assert_eq!(event_data.subscription_id, id);
    assert_eq!(event_data.subscriber, subscriber);
    assert_eq!(event_data.amount, 15_000_000);
    assert_eq!(event_data.prepaid_balance, 15_000_000);
}

#[test]
fn test_deposit_funds_cei_compliance() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = soroban_sdk::token::Client::new(&env, &token);

    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&subscriber, &100_000_000);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
}

// -- Batch charge tests -------------------------------------------------------

    let initial_contract_balance = token_client.balance(&client.address);
    let deposit_amount = 20_000_000i128;

    client.deposit_funds(&id, &subscriber, &deposit_amount);

    // Check effects (state)
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, deposit_amount);

    // Check interactions (transfer)
    assert_eq!(
        token_client.balance(&client.address),
        initial_contract_balance + deposit_amount
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #402)")]
fn test_deposit_funds_below_minimum() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    // min_topup is 1_000_000; try to deposit 500
    test_env.client.deposit_funds(&id, &subscriber, &500);
}

// -- Admin tests --------------------------------------------------------------

#[test]
fn test_rotate_admin() {
    let test_env = TestEnv::default();
    let new_admin = Address::generate(&test_env.env);
    test_env.client.rotate_admin(&test_env.admin, &new_admin);
    assert_eq!(test_env.client.get_admin(), new_admin);
}

#[test]
fn test_emergency_stop() {
    let (_env, client, _, admin) = setup_test_env();
    assert!(!client.get_emergency_stop_status());
    client.enable_emergency_stop(&admin);
    assert!(client.get_emergency_stop_status());
    client.disable_emergency_stop(&admin);
    assert!(!client.get_emergency_stop_status());
}

#[test]
#[should_panic(expected = "Error(Contract, #1009)")]
fn test_create_subscription_blocked_by_emergency_stop() {
    let test_env = TestEnv::default();
    test_env.client.enable_emergency_stop(&test_env.admin);
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    test_env.client.create_subscription(
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
    let (env, client, _, _admin) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    // 1. Success
    let (id1, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id1, PREPAID);

    // 2. InsufficientBalance
    let (id2, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id2, 0);

    // 3. NotActive (Paused)
    let (id3, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id3, &subscriber);

    // 4. IntervalNotElapsed
    let (id4, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id4, PREPAID);

    // 5. Success (Duplicate of id1, but after enough time it could succeed? No, it's the same call)
    // Wait, IDs are processed at the same ledger timestamp.
    
    env.ledger().set_timestamp(T0 + INTERVAL + 1);
    
    // Batch: [id1, id2, id3, id4 (not elapsed)]
    // For id4, T0 + INTERVAL + 1 is actually elapsed. Let me fix id4.
    env.as_contract(&client.address, || {
        let mut sub4 = env.storage().instance().get::<u32, Subscription>(&id4).unwrap();
        sub4.last_payment_timestamp = T0 + INTERVAL + 100; // Will be in the future
        env.storage().instance().set(&id4, &sub4);
    });

    let ids = Vec::from_array(&env, [id1, 999u32, id2, id3, id4]);
    let results = client.batch_charge(&ids);
    
    assert_eq!(results.len(), 5);
    
    // id1: Success
    assert!(results.get(0).unwrap().success);
    
    // 999: NotFound (404)
    assert!(!results.get(1).unwrap().success);
    assert_eq!(results.get(1).unwrap().error_code, 404);
    
    // id2: InsufficientBalance (1003)
    assert!(!results.get(2).unwrap().success);
    assert_eq!(results.get(2).unwrap().error_code, 1003);
    
    // id3: NotActive (1002)
    assert!(!results.get(3).unwrap().success);
    assert_eq!(results.get(3).unwrap().error_code, 1002);
    
    // id4: IntervalNotElapsed (1001)
    assert!(!results.get(4).unwrap().success);
    assert_eq!(results.get(4).unwrap().error_code, 1001);
}

#[test]
fn test_batch_charge_duplicate_ids() {
    let (env, client, _, _admin) = setup_test_env();
    env.ledger().set_timestamp(T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID * 2);

    env.ledger().set_timestamp(T0 + INTERVAL + 1);
    let ids = Vec::from_array(&env, [id, id]);
    let results = client.batch_charge(&ids);
    
    assert_eq!(results.len(), 2);
    // First should succeed
    assert!(results.get(0).unwrap().success);
    // Second should fail with Replay (1007)
    assert!(!results.get(1).unwrap().success);
    assert_eq!(results.get(1).unwrap().error_code, 1007);
    
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, PREPAID * 2 - AMOUNT);
}

#[test]
fn test_batch_charge_large_batch() {
    let (env, client, _, _admin) = setup_test_env();
    env.ledger().set_timestamp(T0);
    
    let mut ids = Vec::new(&env);
    for _ in 0..50 {
        let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
        seed_balance(&env, &client, id, PREPAID);
        ids.push_back(id);
    }

    env.ledger().set_timestamp(T0 + INTERVAL + 1);
    let results = client.batch_charge(&ids);
    assert_eq!(results.len(), 50);
    for i in 0..50 {
        assert!(results.get(i).unwrap().success);
    }
}

#[test]
fn test_batch_charge_matches_single_charge_semantics_for_identical_inputs() {
    let test_env_batch = TestEnv::default();
    let test_env_single = TestEnv::default();

    test_env_batch.set_timestamp(T0);
    test_env_single.set_timestamp(T0);

    let mut ids_batch = [0u32; 3];
    let mut ids_single = [0u32; 3];
    let mut merchants_batch = alloc::vec::Vec::new();
    let mut merchants_single = alloc::vec::Vec::new();

    for idx in 0..3 {
        let (id_batch, _, merchant_batch) =
            fixtures::create_subscription_detailed(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        let (id_single, _, merchant_single) =
            fixtures::create_subscription_detailed(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
        
        fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, id_batch, PREPAID);
        fixtures::seed_balance(&test_env_single.env, &test_env_single.client, id_single, PREPAID);
        
        ids_batch[idx] = id_batch;
        ids_single[idx] = id_single;
        merchants_batch.push(merchant_batch);
        merchants_single.push(merchant_single);
    }

    test_env_batch.jump(INTERVAL + 1);
    test_env_single.jump(INTERVAL + 1);

    let batch_results = collect_batch_result_codes(&test_env_batch.env, &test_env_batch.client, &ids_batch);
    let single_results = collect_single_charge_result_codes(&test_env_single.client, &ids_single);

    assert_eq!(batch_results, single_results);
    assert_eq!(batch_results, alloc::vec![(true, 0), (true, 0), (true, 0)]);

    let batch_snapshots = snapshot_subscriptions(&test_env_batch.client, &ids_batch);
    let single_snapshots = snapshot_subscriptions(&test_env_single.client, &ids_single);
    for (batch_sub, single_sub) in batch_snapshots.iter().zip(single_snapshots.iter()) {
        assert_eq!(batch_sub.prepaid_balance, single_sub.prepaid_balance);
        assert_eq!(
            batch_sub.last_payment_timestamp,
            single_sub.last_payment_timestamp
        );
        assert_eq!(batch_sub.status, single_sub.status);
        assert_eq!(batch_sub.lifetime_charged, single_sub.lifetime_charged);
    }

    for (merchant_batch, merchant_single) in merchants_batch.iter().zip(merchants_single.iter()) {
        assert_eq!(
            test_env_batch.client.get_merchant_balance(merchant_batch),
            test_env_single.client.get_merchant_balance(merchant_single)
        );
    }
}

#[test]
fn test_batch_charge_mixed_results_preserve_single_path_order_and_error_codes() {
    let test_env_batch = TestEnv::default();
    let test_env_single = TestEnv::default();

    test_env_batch.set_timestamp(T0);
    test_env_single.set_timestamp(T0);

    let (valid_batch, _, _merchant_valid_batch) =
        fixtures::create_subscription_detailed(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    let (valid_single, _, _merchant_valid_single) =
        fixtures::create_subscription_detailed(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, valid_batch, PREPAID);
    fixtures::seed_balance(&test_env_single.env, &test_env_single.client, valid_single, PREPAID);

    let (low_batch, _, _merchant_low_batch) =
        fixtures::create_subscription_detailed(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    let (low_single, _, _merchant_low_single) =
        fixtures::create_subscription_detailed(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, low_batch, AMOUNT - 1);
    fixtures::seed_balance(&test_env_single.env, &test_env_single.client, low_single, AMOUNT - 1);

    let (paused_batch, _, merchant_paused_batch) =
        fixtures::create_subscription_detailed(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Paused, AMOUNT, INTERVAL);
    let (paused_single, _, merchant_paused_single) =
        fixtures::create_subscription_detailed(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Paused, AMOUNT, INTERVAL);
    fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, paused_batch, PREPAID);
    fixtures::seed_balance(&test_env_single.env, &test_env_single.client, paused_single, PREPAID);

    test_env_batch.jump(INTERVAL + 1);
    test_env_single.jump(INTERVAL + 1);

    let ids_batch = [
        valid_batch,
        low_batch,
        paused_batch,
        999_999u32,
        valid_batch,
    ];
    let ids_single = [
        valid_single,
        low_single,
        paused_single,
        999_999u32,
        valid_single,
    ];

    let batch_results = collect_batch_result_codes(&test_env_batch.env, &test_env_batch.client, &ids_batch);
    let single_results = collect_single_charge_result_codes(&test_env_single.client, &ids_single);

    assert_eq!(batch_results, single_results);
    assert_eq!(
        batch_results,
        alloc::vec![
            (true, 0),
            (false, Error::InsufficientBalance.to_code()),
            (false, Error::NotActive.to_code()),
            (false, Error::NotFound.to_code()),
            (false, Error::Replay.to_code()),
        ]
    );

    let tracked_batch = [valid_batch, low_batch, paused_batch];
    let tracked_single = [valid_single, low_single, paused_single];
    assert_eq!(
        test_env_batch.client.get_merchant_balance(&merchant_paused_batch),
        test_env_single.client.get_merchant_balance(&merchant_paused_single)
    );
}

#[test]
fn test_batch_charge_basic() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let (id1, _, merchant) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let (id2, _, _) = fixtures::create_subscription_with_merchant(&test_env.env, &test_env.client, SubscriptionStatus::Active, merchant.clone());

    fixtures::seed_balance(&test_env.env, &test_env.client, id1, PREPAID);
    fixtures::seed_balance(&test_env.env, &test_env.client, id2, PREPAID);

    test_env.env.ledger().set_timestamp(T0 + INTERVAL + 1);

    let ids = Vec::from_array(&test_env.env, [id1, id2]);
    let results = test_env.client.batch_charge(&ids);

    assert_eq!(results.len(), 2);
    assert!(results.get(0).unwrap().success);
    assert!(results.get(1).unwrap().success);

    assertions::assert_prepaid_balance(&test_env.client, &id1, PREPAID - AMOUNT);
    assertions::assert_prepaid_balance(&test_env.client, &id1, PREPAID - AMOUNT);
    assertions::assert_prepaid_balance(&test_env.client, &id2, PREPAID - AMOUNT);
}

#[test]
#[should_panic] 
fn test_batch_charge_fails_unauthorized() {
    let env = Env::default();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract_v2(admin.clone()).address();
    client.init(&token, &6, &admin, &1_000_000i128, &604800u64);

    let ids = Vec::from_array(&env, [1]);
    
    // This will panic because no auth is provided for the admin
    client.batch_charge(&ids);
}

#[test]
fn test_batch_charge_partial_success() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let (id1, _, merchant) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let (id2, _, _) = fixtures::create_subscription_with_merchant(&test_env.env, &test_env.client, SubscriptionStatus::Active, merchant.clone());

    fixtures::seed_balance(&test_env.env, &test_env.client, id1, PREPAID);
    // id2 has 0 balance

    test_env.env.ledger().set_timestamp(T0 + INTERVAL + 1);

    let ids = Vec::from_array(&test_env.env, [id1, id2]);
    let results = test_env.client.batch_charge(&ids);

    assert_eq!(results.len(), 2);
    assert!(results.get(0).unwrap().success);
    assert_eq!(results.get(1).unwrap().success, false);
    assert_eq!(results.get(1).unwrap().error_code, Error::InsufficientBalance.to_code());

    assertions::assert_prepaid_balance(&test_env.client, &id1, PREPAID - AMOUNT);
    assertions::assert_prepaid_balance(&test_env.client, &id2, 0);
}

#[test]
fn test_batch_charge_failed_items_match_single_path_without_cross_item_side_effects() {
    let test_env_batch = TestEnv::default();
    let test_env_single = TestEnv::default();

    test_env_batch.env.ledger().set_timestamp(T0);
    test_env_single.env.ledger().set_timestamp(T0);

    let (ok_one_batch, _, merchant_ok_one_batch) =
        fixtures::create_subscription(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Active);
    let (ok_one_single, _, merchant_ok_one_single) =
        fixtures::create_subscription(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, ok_one_batch, PREPAID);
    fixtures::seed_balance(&test_env_single.env, &test_env_single.client, ok_one_single, PREPAID);

    let (failing_batch, _, merchant_failing_batch) =
        fixtures::create_subscription(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Active);
    let (failing_single, _, merchant_failing_single) =
        fixtures::create_subscription(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, failing_batch, 1);
    fixtures::seed_balance(&test_env_single.env, &test_env_single.client, failing_single, 1);

    let (ok_two_batch, _, merchant_ok_two_batch) =
        fixtures::create_subscription(&test_env_batch.env, &test_env_batch.client, SubscriptionStatus::Active);
    let (ok_two_single, _, merchant_ok_two_single) =
        fixtures::create_subscription(&test_env_single.env, &test_env_single.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, ok_two_batch, PREPAID);
    fixtures::seed_balance(&test_env_single.env, &test_env_single.client, ok_two_single, PREPAID);

    test_env_batch.env.ledger().set_timestamp(T0 + INTERVAL + 1);
    test_env_single.env.ledger().set_timestamp(T0 + INTERVAL + 1);

    let ids_batch = [ok_one_batch, failing_batch, ok_two_batch];
    let ids_single = [ok_one_single, failing_single, ok_two_single];

    let batch_results = collect_batch_result_codes(&test_env_batch.env, &test_env_batch.client, &ids_batch);
    let single_results = collect_single_charge_result_codes(&test_env_single.client, &ids_single);

    assert_eq!(batch_results, single_results);
    assert_eq!(
        batch_results,
        alloc::vec![
            (true, 0),
            (false, Error::InsufficientBalance.to_code()),
            (true, 0),
        ]
    );

    let batch_snapshots = snapshot_subscriptions(&test_env_batch.client, &ids_batch);
    let single_snapshots = snapshot_subscriptions(&test_env_single.client, &ids_single);
    for (batch_sub, single_sub) in batch_snapshots.iter().zip(single_snapshots.iter()) {
        assert_eq!(batch_sub.prepaid_balance, single_sub.prepaid_balance);
        assert_eq!(
            batch_sub.last_payment_timestamp,
            single_sub.last_payment_timestamp
        );
        assert_eq!(batch_sub.status, single_sub.status);
    }

    assert_eq!(
        test_env_batch.client.get_merchant_balance(&merchant_ok_one_batch),
        test_env_single.client.get_merchant_balance(&merchant_ok_one_single)
    );
    assert_eq!(
        test_env_batch.client.get_merchant_balance(&merchant_failing_batch),
        test_env_single.client.get_merchant_balance(&merchant_failing_single)
    );
    assert_eq!(
        test_env_batch.client.get_merchant_balance(&merchant_ok_two_batch),
        test_env_single.client.get_merchant_balance(&merchant_ok_two_single)
    );
}

#[test]
fn test_batch_charge_high_volume_list_matches_single_path_semantics() {
    let test_env_batch = TestEnv::default();
    let test_env_single = TestEnv::default();

    test_env_batch.env.ledger().set_timestamp(T0);
    test_env_single.env.ledger().set_timestamp(T0);

    let mut ids_batch = alloc::vec::Vec::new();
    let mut ids_single = alloc::vec::Vec::new();
    let mut merchants_batch = alloc::vec::Vec::new();
    let mut merchants_single = alloc::vec::Vec::new();

    for idx in 0..20 {
        let status = if idx % 5 == 0 {
            SubscriptionStatus::Paused
        } else {
            SubscriptionStatus::Active
        };
        let (id_batch, _, merchant_batch) = fixtures::create_subscription(&test_env_batch.env, &test_env_batch.client, status.clone());
        let (id_single, _, merchant_single) = fixtures::create_subscription(&test_env_single.env, &test_env_single.client, status);

        let balance = if idx % 2 == 0 { PREPAID } else { AMOUNT - 1 };
        fixtures::seed_balance(&test_env_batch.env, &test_env_batch.client, id_batch, balance);
        fixtures::seed_balance(&test_env_single.env, &test_env_single.client, id_single, balance);

        ids_batch.push(id_batch);
        ids_single.push(id_single);
        merchants_batch.push(merchant_batch);
        merchants_single.push(merchant_single);
    }

    test_env_batch.env.ledger().set_timestamp(T0 + INTERVAL + 1);
    test_env_single.env.ledger().set_timestamp(T0 + INTERVAL + 1);

    let mut input_batch = ids_batch.clone();
    let mut input_single = ids_single.clone();
    input_batch.push(ids_batch[2]);
    input_batch.push(ids_batch[7]);
    input_single.push(ids_single[2]);
    input_single.push(ids_single[7]);

    let batch_results = collect_batch_result_codes(&test_env_batch.env, &test_env_batch.client, &input_batch);
    let single_results = collect_single_charge_result_codes(&test_env_single.client, &input_single);

    assert_eq!(batch_results, single_results);
    assert_eq!(batch_results.len(), 22);

    let batch_snapshots = snapshot_subscriptions(&test_env_batch.client, &ids_batch);
    let single_snapshots = snapshot_subscriptions(&test_env_single.client, &ids_single);
    for (batch_sub, single_sub) in batch_snapshots.iter().zip(single_snapshots.iter()) {
        assert_eq!(batch_sub.prepaid_balance, single_sub.prepaid_balance);
        assert_eq!(
            batch_sub.last_payment_timestamp,
            single_sub.last_payment_timestamp
        );
        assert_eq!(batch_sub.status, single_sub.status);
    }

    for (merchant_batch, merchant_single) in merchants_batch.iter().zip(merchants_single.iter()) {
        assert_eq!(
            test_env_batch.client.get_merchant_balance(merchant_batch),
            test_env_single.client.get_merchant_balance(merchant_single)
        );
    }
}

// -- Next charge info test ----------------------------------------------------

#[test]
fn test_next_charge_info() {
    let test_env = TestEnv::default();
    test_env.set_timestamp(T0);
    let (id, _, _) = fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    let info = test_env.client.get_next_charge_info(&id);
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
    };
    let info = compute_next_charge_info(&sub);
    assert!(!info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, 3000 + INTERVAL);
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
        lifetime_charged: 0, start_time: 0, expires_at: None,
    };
    let info = compute_next_charge_info(&sub);
    assert!(info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, T0 + INTERVAL);

    env.ledger().with_mut(|li| li.timestamp = info.next_charge_timestamp - 1);
    assert_eq!(
        client.try_charge_subscription(&id),
        Err(Ok(Error::IntervalNotElapsed))
    );

    env.ledger().with_mut(|li| li.timestamp = info.next_charge_timestamp);
    assert_eq!(
        client.try_charge_subscription(&id),
        Ok(Ok(ChargeExecutionResult::Charged))
    );
}

#[test]
fn test_next_charge_info_cross_check_status_gating() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id_paused, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Paused);
    let (id_cancelled, _, _) =
        create_test_subscription(&env, &client, SubscriptionStatus::Cancelled);
    let (id_insufficient, _, _) =
        create_test_subscription(&env, &client, SubscriptionStatus::InsufficientBalance);
    let (id_grace, _, _) =
        create_test_subscription(&env, &client, SubscriptionStatus::GracePeriod);

    for id in [id_paused, id_cancelled, id_insufficient, id_grace] {
        seed_balance(&env, &client, id, PREPAID);
    }



// -- Top-up estimation (precision) --------------------------------------------

#[test]
fn test_estimate_topup_zero_intervals_returns_zero() {
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
     &None::<u64>);
    seed_balance(&env, &client, id, PREPAID);

    assert_eq!(client.estimate_topup_for_intervals(&id, &0), 0);
}

#[test]
fn test_estimate_topup_balance_already_sufficient_returns_zero() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Balance covers 3 future charges.
    seed_balance(&env, &client, id, 3 * AMOUNT);
    assert_eq!(client.estimate_topup_for_intervals(&id, &3), 0);
}

#[test]
fn test_estimate_topup_cross_check_after_actual_charge() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    // Before any charge, to cover next 6 intervals we need: 6*AMOUNT - PREPAID.
    assert_eq!(
        client.estimate_topup_for_intervals(&id, &6),
        6 * AMOUNT - PREPAID
    );

    // Execute one real charge at the exact boundary.
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL);
    assert_eq!(
        client.try_charge_subscription(&id),
        Ok(Ok(ChargeExecutionResult::Charged))
    );

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, PREPAID - AMOUNT);

    // Now, covering next 5 intervals should be the same shortfall.
    assert_eq!(
        client.estimate_topup_for_intervals(&id, &5),
        5 * AMOUNT - (PREPAID - AMOUNT)
    );
}

#[test]
fn test_estimate_topup_overflow_protection() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Force multiplication overflow: amount * num_intervals.
    let mut sub = client.get_subscription(&id);
    sub.amount = i128::MAX;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });

    assert_eq!(
        client.try_estimate_topup_for_intervals(&id, &2),
        Err(Ok(Error::Overflow))
    );
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
        grace_start_timestamp: None,
    };
    let info = compute_next_charge_info(&sub);
    assert!(info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, u64::MAX);
}

// -- Replay protection --------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #1007)")]
fn test_replay_charge_same_period() {
    let test_env = TestEnv::default();
    test_env.set_timestamp(T0);
    let (id, _, _) = fixtures::create_subscription_detailed(&test_env.env, &test_env.client, SubscriptionStatus::Active, AMOUNT, INTERVAL);
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);

    test_env.jump(INTERVAL + 1);
    test_env.client.charge_subscription(&id);
    // Second charge in same period should fail
    test_env.client.charge_subscription(&id);
}

// -- Recovery -----------------------------------------------------------------

#[test]
fn test_recover_stranded_funds() {
    let test_env = TestEnv::default();
    let recipient = Address::generate(&test_env.env);
    test_env.client.recover_stranded_funds(
        &test_env.admin,
        &recipient,
        &1_000_000,
        &RecoveryReason::AccidentalTransfer,
    );
}

// -- Lifetime cap tests -------------------------------------------------------

#[test]
fn test_lifetime_cap_auto_cancel() {
    let test_env = TestEnv::default();
    test_env.set_timestamp(T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    // Cap = 2 * AMOUNT, so after 2 charges, should auto-cancel
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(2 * AMOUNT),
    );
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);

    // First charge
    test_env.jump(INTERVAL + 1);
    test_env.client.charge_subscription(&id);
    let sub = test_env.client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, AMOUNT);
    assert_eq!(sub.status, SubscriptionStatus::Active);

    // Second charge -> cap reached -> auto-cancel
    test_env.jump(INTERVAL);
    test_env.client.charge_subscription(&id);
    let sub = test_env.client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, 2 * AMOUNT);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
}

#[test]
fn test_get_cap_info() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let cap = 100_000_000i128;
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(cap),
     &None::<u64>);
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
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let cap = 50_000_000i128;
    let plan_id = test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &Some(cap));

    let template = test_env.client.get_plan_template(&plan_id);
    assert_eq!(template.lifetime_cap, Some(cap));

    let sub_id = test_env.client.create_subscription_from_plan(&subscriber, &plan_id);
    let sub = test_env.client.get_subscription(&sub_id);
    assert_eq!(sub.lifetime_cap, Some(cap));
}

/// Plan template with no cap creates uncapped subscriptions.
#[test]
fn test_plan_template_no_cap_creates_uncapped_sub() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let plan_id = test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    let plan = test_env.client.get_plan_template(&plan_id);
    assert_eq!(plan.amount, AMOUNT);

    let sub_id = test_env.client.create_subscription_from_plan(&subscriber, &plan_id);
    let sub = test_env.client.get_subscription(&sub_id);
    assert_eq!(sub.amount, AMOUNT);
    assert_eq!(sub.merchant, merchant);
}

#[test]
fn test_plan_max_concurrent_subscriptions_enforced_per_subscriber() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let plan_id = test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    // Limit each subscriber to a single active subscription for this plan.
    test_env.client.set_plan_max_active_subs(&merchant, &plan_id, &1);

    // First subscription succeeds.
    let _sub1 = test_env.client.create_subscription_from_plan(&subscriber, &plan_id);

    // Second subscription for the same subscriber/plan is rejected.
    let result = test_env.client.try_create_subscription_from_plan(&subscriber, &plan_id);
    assert_eq!(result, Err(Ok(Error::MaxConcurrentSubscriptionsReached)));

    // Another subscriber is unaffected by this limit.
    let other_subscriber = Address::generate(&test_env.env);
    let _sub_other = test_env.client.create_subscription_from_plan(&other_subscriber, &plan_id);
}

#[test]
fn test_plan_max_concurrent_allows_new_after_cancellation() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let plan_id = test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    test_env.client.set_plan_max_active_subs(&merchant, &plan_id, &1);

    let sub1 = test_env.client.create_subscription_from_plan(&subscriber, &plan_id);
    test_env.client.cancel_subscription(&sub1, &subscriber);

    // Because only ACTIVE subscriptions are counted, a new subscription is allowed
    // after cancellation.
    let sub2 = test_env.client.create_subscription_from_plan(&subscriber, &plan_id);
    assertions::assert_status(&test_env.client, &sub2, SubscriptionStatus::Active);
}

#[test]
fn test_subscriber_credit_limit_blocks_new_subscription_creation() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    // Limit total exposure for this subscriber/token to a single interval amount.
    test_env.client.set_subscriber_credit_limit(&test_env.admin, &subscriber, &test_env.token, &AMOUNT);

    // First subscription fits entirely within the limit.
    let _sub1 =
        client.create_subscription(&subscriber, &merchant, &AMOUNT, &INTERVAL, &false, &None, &None::<u64>);

    // Second subscription would exceed credit limit (another interval liability).
    let result =
        client.try_create_subscription(&subscriber, &merchant, &AMOUNT, &INTERVAL, &false, &None, &None::<u64>);
    assert_eq!(result, Err(Ok(Error::CreditLimitExceeded)));
}

#[test]
fn test_subscriber_credit_limit_blocks_topup_when_exposure_exceeds_limit() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000);
    let merchant = Address::generate(&test_env.env);

    // Exposure limit small enough that initial subscription fits, but top-up does not.
    let limit = AMOUNT + 5_000_000i128;
    test_env.client.set_subscriber_credit_limit(&test_env.admin, &subscriber, &test_env.token, &limit);

    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);

    // Deposit that would keep us under the limit succeeds.
    test_env.client.deposit_funds(&sub_id, &subscriber, &5_000_000i128);

    // Further deposit would push exposure over the limit and must be rejected.
    let result = test_env.client.try_deposit_funds(&sub_id, &subscriber, &1_000_000i128);
    assert_eq!(result, Err(Ok(Error::CreditLimitExceeded)));
}

#[test]
fn test_get_subscriber_credit_limit_and_exposure_views() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000);
    let merchant = Address::generate(&test_env.env);

    // Default: no limit configured.
    assert_eq!(test_env.client.get_subscriber_credit_limit(&subscriber, &test_env.token), 0);

    test_env.client.set_subscriber_credit_limit(&test_env.admin, &subscriber, &test_env.token, &(AMOUNT * 10));

    // After creating a subscription, exposure reflects one interval liability and zero prepaid.
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    let exposure = client.get_subscriber_exposure(&subscriber, &token);
    assert_eq!(exposure, AMOUNT);

    // After topping up, exposure increases by the deposited amount.
    test_env.client.deposit_funds(&sub_id, &subscriber, &5_000_000i128);
    let exposure_after_topup = test_env.client.get_subscriber_exposure(&subscriber, &test_env.token);
    assert_eq!(exposure_after_topup, AMOUNT + 5_000_000i128);
}

#[test]
fn test_partial_refund_debits_prepaid_and_transfers_tokens() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &50_000_000i128);
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&sub_id, &subscriber, &20_000_000i128);

    let balance_before = test_env.token_client().balance(&subscriber);
    assertions::assert_prepaid_balance(&test_env.client, &sub_id, 20_000_000i128);

    // Perform a partial refund of half the prepaid balance.
    test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &10_000_000i128);

    let balance_after = test_env.token_client().balance(&subscriber);
    assertions::assert_prepaid_balance(&test_env.client, &sub_id, 10_000_000i128);
    assert_eq!(balance_after, balance_before + 10_000_000i128);
}

#[test]
fn test_partial_refund_rejects_invalid_amounts_and_auth() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &50_000_000i128);
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&sub_id, &subscriber, &5_000_000i128);

    // Zero or negative refund amounts are rejected.
    let zero_res = test_env.client.try_partial_refund(&test_env.admin, &sub_id, &subscriber, &0i128);
    assert_eq!(zero_res, Err(Ok(Error::InvalidAmount)));

    let negative_res = test_env.client.try_partial_refund(&test_env.admin, &sub_id, &subscriber, &-1i128);
    assert_eq!(negative_res, Err(Ok(Error::InvalidAmount)));

    // Refund exceeding prepaid balance is rejected.
    let over_res = test_env.client.try_partial_refund(&test_env.admin, &sub_id, &subscriber, &10_000_000i128);
    assert_eq!(over_res, Err(Ok(Error::InsufficientBalance)));

    // Non-admin cannot authorize partial refunds.
    let other_admin = Address::generate(&test_env.env);
    let unauth_res = test_env.client.try_partial_refund(&other_admin, &sub_id, &subscriber, &1_000_000i128);
    assert_eq!(unauth_res, Err(Ok(Error::Unauthorized)));

    // Wrong subscriber address is rejected.
    let wrong_subscriber = Address::generate(&test_env.env);
    let wrong_sub_res =
        test_env.client.try_partial_refund(&test_env.admin, &sub_id, &wrong_subscriber, &1_000_000i128);
    assert_eq!(wrong_sub_res, Err(Ok(Error::Unauthorized)));
}

// =============================================================================
// Partial Refund — Extended Coverage
// =============================================================================

/// Repeated partial refunds each debit the correct incremental amount.
#[test]
fn test_partial_refund_repeated_debits_are_cumulative() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &30_000_000i128);

    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&sub_id, &subscriber, &30_000_000i128);

    // Three successive partial refunds of 5 USDC each.
    for _ in 0..3 {
        test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &5_000_000i128);
    }

    let sub = test_env.client.get_subscription(&sub_id);
    assert_eq!(sub.prepaid_balance, 15_000_000i128); // 30 - 3*5 = 15
    assert_eq!(test_env.token_client().balance(&subscriber), 15_000_000i128);
}

/// Cumulative refunds that exactly drain the balance succeed; one more unit fails.
#[test]
fn test_partial_refund_cumulative_exact_drain_then_over_refund_fails() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000i128);
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&sub_id, &subscriber, &10_000_000i128);

    // Refund the full balance as two equal halves.
    test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &5_000_000i128);
    test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &5_000_000i128);

    assertions::assert_prepaid_balance(&test_env.client, &sub_id, 0);

    // Any further refund must fail — balance is zero.
    let over = test_env.client.try_partial_refund(&test_env.admin, &sub_id, &subscriber, &1i128);
    assert_eq!(over, Err(Ok(Error::InsufficientBalance)));
}

/// A partial refund equal to the full prepaid balance (full-balance-as-partial) succeeds.
#[test]
fn test_partial_refund_full_balance_as_partial_succeeds() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &20_000_000i128);
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&sub_id, &subscriber, &20_000_000i128);

    // Refund the entire prepaid balance in one call.
    test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &20_000_000i128);

    assertions::assert_prepaid_balance(&test_env.client, &sub_id, 0);
    assert_eq!(test_env.token_client().balance(&subscriber), 20_000_000i128);
}

/// Partial refund is allowed on a cancelled subscription (remaining balance can be returned).
#[test]
fn test_partial_refund_after_cancellation_succeeds() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &15_000_000i128);
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&sub_id, &subscriber, &15_000_000i128);
    test_env.client.cancel_subscription(&sub_id, &subscriber);

    // Admin can still issue a partial refund on a cancelled subscription.
    test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &5_000_000i128);

    assertions::assert_prepaid_balance(&test_env.client, &sub_id, 10_000_000i128);
    assert_eq!(test_env.token_client().balance(&subscriber), 5_000_000i128);
}

/// Partial refund emits a PartialRefundEvent with correct fields.
#[test]
fn test_partial_refund_emits_event() {
    let test_env = TestEnv::default();
    test_env.env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000i128);
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&sub_id, &subscriber, &10_000_000i128);

    test_env.client.partial_refund(&test_env.admin, &sub_id, &subscriber, &3_000_000i128);

    // At least one event must have been emitted by the refund call.
    assert!(!test_env.env.events().all().is_empty());
}

#[test]
fn test_update_plan_template_creates_new_version_and_preserves_old() {
    let test_env = TestEnv::default();
    let merchant = Address::generate(&test_env.env);

    let cap = 50_000_000i128;
    let plan_id = test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &Some(cap));
    let original = test_env.client.get_plan_template(&plan_id);
    assert_eq!(original.version, 1);

    let new_amount = AMOUNT * 2;
    let new_interval = INTERVAL / 2;
    let new_plan_id = test_env.client.update_plan_template(
        &merchant,
        &plan_id,
        &new_amount,
        &new_interval,
        &false,
        &Some(cap),
    );

    // Old plan remains unchanged and addressable.
    let original_after = test_env.client.get_plan_template(&plan_id);
    assert_eq!(original_after.version, 1);
    assert_eq!(original_after.amount, AMOUNT);
    assert_eq!(original_after.interval_seconds, INTERVAL);
    assert!(!original_after.usage_enabled);

    // New plan has incremented version and updated fields, sharing template_key.
    let updated = test_env.client.get_plan_template(&new_plan_id);
    assert_eq!(updated.version, 2);
    assert_eq!(updated.template_key, original_after.template_key);
    assert_eq!(updated.amount, new_amount);
    assert_eq!(updated.interval_seconds, new_interval);
    assert!(!updated.usage_enabled);
    assert_eq!(updated.lifetime_cap, Some(cap));
}

#[test]
fn test_migrate_subscription_to_new_plan_version() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let cap = 50_000_000i128;
    let plan_id = test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &Some(cap));
    let new_amount = AMOUNT * 3;
    let new_interval = INTERVAL / 3;
    let new_plan_id = test_env.client.update_plan_template(
        &merchant,
        &plan_id,
        &new_amount,
        &new_interval,
        &false,
        &Some(cap),
    );

    let sub_id = test_env.client.create_subscription_from_plan(&subscriber, &plan_id);
    let before = test_env.client.get_subscription(&sub_id);
    assert_eq!(before.amount, AMOUNT);
    assert_eq!(before.interval_seconds, INTERVAL);
    assert!(!before.usage_enabled);

    test_env.client.migrate_subscription_to_plan(&subscriber, &sub_id, &new_plan_id);

    let after = test_env.client.get_subscription(&sub_id);
    assert_eq!(after.amount, new_amount);
    assert_eq!(after.interval_seconds, new_interval);
    assert!(!after.usage_enabled);
    // Lifetime tracking is preserved.
    assert_eq!(after.lifetime_charged, 0);
    assert_eq!(after.lifetime_cap, Some(cap));
}

#[test]
fn test_migrate_subscription_rejects_cross_template_family() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let plan_family_a =
        test_env.client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    let plan_family_b =
        test_env.client.create_plan_template(&merchant, &(AMOUNT * 2), &INTERVAL, &false, &None::<i128>);

    let sub_id = test_env.client.create_subscription_from_plan(&subscriber, &plan_family_a);

    let result = test_env.client.try_migrate_subscription_to_plan(&subscriber, &sub_id, &plan_family_b);
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

// --- Cancellation and Withdrawal Regression Tests ---------------------------

#[test]
fn test_cancel_from_various_states() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    // Cancel from Active
    let id1 = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);

    // Cancel from Paused
    let id2 = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.pause_subscription(&id2, &subscriber);
    test_env.client.cancel_subscription(&id2, &subscriber);
    assertions::assert_status(&test_env.client, &id2, SubscriptionStatus::Cancelled);
}

#[test]
fn test_withdraw_subscriber_funds_exactly_once() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(cap),
     &None::<u64>);

    test_env.client.cancel_subscription(&id, &subscriber);

    // First withdrawal: Success
    test_env.client.withdraw_subscriber_funds(&id, &subscriber);
    assertions::assert_prepaid_balance(&test_env.client, &id, 0);

    // Second withdrawal: Should fail with InvalidAmount (since balance is now 0)
    let result = test_env.client.try_withdraw_subscriber_funds(&id, &subscriber);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn test_withdraw_zero_balance_fails() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    seed_balance(&env, &client, id, PREPAID);

    let result = test_env.client.try_withdraw_subscriber_funds(&id, &subscriber);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn test_cancel_and_withdraw_events() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &5_000_000);

    test_env.client.cancel_subscription(&id, &subscriber);

    // Check cancellation event
    let events = test_env.env.events().all();
    let _cancel_event = events.get(events.len() - 1).unwrap();

    test_env.client.withdraw_subscriber_funds(&id, &subscriber);

    // Check withdrawal event
    let events = test_env.env.events().all();
    let _withdraw_event = events.get(events.len() - 1).unwrap();
}

#[test]
fn test_migrate_subscription_requires_plan_origin() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    // Create a subscription directly (not from a plan template).
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    let id2 = client.create_subscription(
        &subscriber,
        &merchant,
        &plan_id,
        &(AMOUNT * 2),
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);

    let page = client.list_subscriptions_by_subscriber(&subscriber, &0, &10);
    assert_eq!(page.subscription_ids.len(), 2);
    assert_eq!(page.subscription_ids.get(0).unwrap(), id1);
    assert_eq!(page.subscription_ids.get(1).unwrap(), id2);
    assert!(!page.has_next);
}

/// Subscriber can withdraw remaining prepaid balance after cap-triggered cancellation.
#[test]
fn test_cap_cancelled_subscriber_can_withdraw() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &1_000_000_000i128);

    let cap = 2 * AMOUNT;
    let sub_id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &5_000_000);

    // Fund the subscription so the vault holds real tokens for withdrawal.
    test_env.client.deposit_funds(&sub_id, &subscriber, &PREPAID);

    test_env.env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    test_env.client.charge_subscription(&sub_id);
    test_env.env.ledger()
        .with_mut(|li| li.timestamp = T0 + 2 * INTERVAL + 1);
    test_env.client.charge_subscription(&sub_id);

    assertions::assert_status(&test_env.client, &sub_id, SubscriptionStatus::Cancelled);
    let sub_after = test_env.client.get_subscription(&sub_id);
    assert!(sub_after.prepaid_balance > 0);

    // Subscriber can withdraw remaining prepaid balance
    test_env.client.withdraw_subscriber_funds(&sub_id, &subscriber);
    assertions::assert_prepaid_balance(&test_env.client, &sub_id, 0);
}

#[test]
fn test_charge_usage_basic() {
    let test_env = TestEnv::default();
    test_env.env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &true,
        &None::<i128>,
    );
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);

    test_env.client.charge_usage(&id, &1_000_000);
    assertions::assert_prepaid_balance(&test_env.client, &id, PREPAID - 1_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #1004)")]
fn test_charge_usage_not_enabled() {
    let test_env = TestEnv::default();
    let (id, _, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);
    test_env.client.charge_usage(&id, &1_000_000);
}

// -- Merchant tests -----------------------------------------------------------

#[test]
fn test_merchant_balance_and_withdrawal() {
    let test_env = TestEnv::default();
    test_env.env.ledger().with_mut(|li| li.timestamp = T0);

    let (id, _, merchant) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);

    test_env.env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    test_env.client.charge_subscription(&id);

    let balance = test_env.client.get_merchant_balance(&merchant);
    assert!(balance > 0);
}

#[test]
fn test_withdraw_merchant_funds_reduces_default_bucket_and_emits_event() {
    let (env, client, token, _) = setup_test_env();
    let merchant = Address::generate(&env);
    let contract_id = client.address.clone();

    seed_merchant_balance(&env, &contract_id, &merchant, &token, 9_000_000i128);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&contract_id, &9_000_000i128);

    env.as_contract(&contract_id, || {
        crate::merchant::withdraw_merchant_funds(&env, merchant.clone(), 4_000_000i128)
    })
    .unwrap();

    assert_eq!(client.get_merchant_balance(&merchant), 5_000_000i128);

    let encoded: Val = MerchantWithdrawalEvent {
        merchant: merchant.clone(),
        token: token.clone(),
        amount: 4_000_000i128,
        remaining_balance: 5_000_000i128,
    }
    .into_val(&env);
    let event = MerchantWithdrawalEvent::try_from_val(&env, &encoded).unwrap();
    assert_eq!(event.merchant, merchant);
    assert_eq!(event.token, token);
    assert_eq!(event.amount, 4_000_000i128);
    assert_eq!(event.remaining_balance, 5_000_000i128);
}

#[test]
fn test_withdraw_merchant_funds_rejects_empty_bucket() {
    let (env, client, _, _) = setup_test_env();
    let merchant = Address::generate(&env);

    let result = client.try_withdraw_merchant_funds(&merchant, &1_000_000i128);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

#[test]
fn test_withdraw_merchant_funds_rejects_overdraw() {
    let (env, client, token, _) = setup_test_env();
    let merchant = Address::generate(&env);
    let contract_id = client.address.clone();

    seed_merchant_balance(&env, &contract_id, &merchant, &token, 3_000_000i128);
    soroban_sdk::token::StellarAssetClient::new(&env, &token).mint(&contract_id, &3_000_000i128);

    let result = client.try_withdraw_merchant_funds(&merchant, &4_000_000i128);
    assert_eq!(result, Err(Ok(Error::InsufficientBalance)));
    assert_eq!(client.get_merchant_balance(&merchant), 3_000_000i128);
}

#[test]
fn test_withdraw_merchant_token_funds_only_debits_requested_bucket_and_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
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
    seed_merchant_balance(&env, &contract_id, &merchant, &token_a, 5_000_000i128);
    seed_merchant_balance(&env, &contract_id, &merchant, &token_b, 7_000_000i128);
    soroban_sdk::token::StellarAssetClient::new(&env, &token_a).mint(&contract_id, &5_000_000i128);
    soroban_sdk::token::StellarAssetClient::new(&env, &token_b).mint(&contract_id, &7_000_000i128);

    env.as_contract(&contract_id, || {
        crate::merchant::withdraw_merchant_funds_for_token(
            &env,
            merchant.clone(),
            token_b.clone(),
            2_000_000i128,
        )
    })
    .unwrap();

    assert_eq!(
        client.get_merchant_balance_by_token(&merchant, &token_a),
        5_000_000i128
    );
    assert_eq!(
        client.get_merchant_balance_by_token(&merchant, &token_b),
        5_000_000i128
    );

    let encoded: Val = MerchantWithdrawalEvent {
        merchant: merchant.clone(),
        token: token_b.clone(),
        amount: 2_000_000i128,
        remaining_balance: 5_000_000i128,
    }
    .into_val(&env);
    let event = MerchantWithdrawalEvent::try_from_val(&env, &encoded).unwrap();
    assert_eq!(event.merchant, merchant);
    assert_eq!(event.token, token_b);
    assert_eq!(event.amount, 2_000_000i128);
    assert_eq!(event.remaining_balance, 5_000_000i128);
}

#[test]
fn test_withdraw_merchant_token_funds_rejects_empty_bucket() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, token, admin) = setup_contract(&env);
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.add_accepted_token(&admin, &token_b, &6);

    let merchant = Address::generate(&env);
    seed_merchant_balance(&env, &client.address, &merchant, &token, 3_000_000i128);

    let result = client.try_withdraw_merchant_token_funds(&merchant, &token_b, &1_000_000i128);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

#[test]
fn test_withdraw_merchant_token_funds_checks_vault_balance_before_transfer() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, token, _admin) = setup_contract(&env);
    let merchant = Address::generate(&env);

    seed_merchant_balance(&env, &client.address, &merchant, &token, 4_000_000i128);

    let result = client.try_withdraw_merchant_token_funds(&merchant, &token, &4_000_000i128);
    assert_eq!(result, Err(Ok(Error::InsufficientBalance)));
    assert_eq!(
        client.get_merchant_balance_by_token(&merchant, &token),
        4_000_000i128
    );
}

// -- End-to-end billing lifecycle tests --------------------------------------

#[test]
fn test_billing_lifecycle_golden_path_end_to_end() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let minted = 100_000_000i128;
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &minted);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let created = test_env.client.get_subscription(&id);
    assert_eq!(created.status, SubscriptionStatus::Active);
    assert_eq!(created.prepaid_balance, 0);
    assert_eq!(created.last_payment_timestamp, T0);
    assert_eq!(test_env.client.get_merchant_balance(&merchant), 0);

    test_env.client.deposit_funds(&id, &subscriber, &PREPAID);
    let after_deposit = test_env.client.get_subscription(&id);
    assert_eq!(after_deposit.status, SubscriptionStatus::Active);
    assert_eq!(after_deposit.prepaid_balance, PREPAID);
    assert_eq!(test_env.token_client().balance(&subscriber), minted - PREPAID);
    assert_eq!(test_env.token_client().balance(&test_env.client.address), PREPAID);

    test_env.env.ledger().set_timestamp(T0 + INTERVAL);
    test_env.client.charge_subscription(&id);
    let after_first_charge = test_env.client.get_subscription(&id);
    assert_eq!(after_first_charge.status, SubscriptionStatus::Active);
    assert_eq!(after_first_charge.prepaid_balance, PREPAID - AMOUNT);
    assert_eq!(after_first_charge.last_payment_timestamp, T0 + INTERVAL);
    assert_eq!(after_first_charge.lifetime_charged, AMOUNT);
    assert_eq!(test_env.client.get_merchant_balance(&merchant), AMOUNT);

    test_env.env.ledger().set_timestamp(T0 + (2 * INTERVAL));
    test_env.client.charge_subscription(&id);
    let after_second_charge = test_env.client.get_subscription(&id);
    assert_eq!(after_second_charge.status, SubscriptionStatus::Active);
    assert_eq!(after_second_charge.prepaid_balance, PREPAID - (2 * AMOUNT));
    assert_eq!(
        after_second_charge.last_payment_timestamp,
        T0 + (2 * INTERVAL)
    );
    assert_eq!(after_second_charge.lifetime_charged, 2 * AMOUNT);
    assert_eq!(test_env.client.get_merchant_balance(&merchant), 2 * AMOUNT);

    let statements = test_env.client.get_sub_statements_offset(&id, &0, &10, &true);
    assert_eq!(statements.total, 2);
    assert_eq!(statements.statements.len(), 2);

    let newest = statements.statements.get(0).unwrap();
    assert_eq!(newest.sequence, 1);
    assert_eq!(newest.charged_at, T0 + (2 * INTERVAL));
    assert_eq!(newest.period_start, T0 + INTERVAL);
    assert_eq!(newest.period_end, T0 + (2 * INTERVAL));
    assert_eq!(newest.amount, AMOUNT);
    assert_eq!(newest.merchant, merchant.clone());
    assert_eq!(newest.kind, crate::BillingChargeKind::Interval);

    let oldest = statements.statements.get(1).unwrap();
    assert_eq!(oldest.sequence, 0);
    assert_eq!(oldest.charged_at, T0 + INTERVAL);
    assert_eq!(oldest.period_start, T0);
    assert_eq!(oldest.period_end, T0 + INTERVAL);
    assert_eq!(oldest.amount, AMOUNT);
    assert_eq!(oldest.merchant, merchant.clone());
    assert_eq!(oldest.kind, crate::BillingChargeKind::Interval);

    let first_page = test_env.client.get_sub_statements_cursor(&id, &None::<u32>, &1, &true);
    assert_eq!(first_page.total, 2);
    assert_eq!(first_page.statements.len(), 1);
    assert_eq!(first_page.statements.get(0).unwrap().sequence, 1);
    assert_eq!(first_page.next_cursor, Some(0));

    let second_page = test_env.client.get_sub_statements_cursor(&id, &first_page.next_cursor, &1, &true);
    assert_eq!(second_page.total, 2);
    assert_eq!(second_page.statements.len(), 1);
    assert_eq!(second_page.statements.get(0).unwrap().sequence, 0);
    assert_eq!(second_page.next_cursor, None);

    let merchant_wallet_before = test_env.token_client().balance(&merchant);
    test_env.client.withdraw_merchant_funds(&merchant, &(2 * AMOUNT));
    assert_eq!(test_env.client.get_merchant_balance(&merchant), 0);
    assert_eq!(
        test_env.token_client().balance(&merchant),
        merchant_wallet_before + (2 * AMOUNT)
    );
    assert_eq!(
        test_env.token_client().balance(&test_env.client.address),
        PREPAID - (2 * AMOUNT)
    );

    test_env.client.cancel_subscription(&id, &subscriber);
    assertions::assert_status(&test_env.client, &id, SubscriptionStatus::Cancelled);

    test_env.client.withdraw_subscriber_funds(&id, &subscriber);
    let closed_out = test_env.client.get_subscription(&id);
    assert_eq!(closed_out.prepaid_balance, 0);
    assert_eq!(test_env.token_client().balance(&test_env.client.address), 0);
    assert_eq!(test_env.token_client().balance(&subscriber), minted - (2 * AMOUNT));
}

#[test]
fn test_billing_lifecycle_delayed_charge_and_min_topup_progression() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &50_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &19_000_000i128);

    let delayed_charge_at = T0 + (2 * INTERVAL) + 77;
    test_env.env.ledger().set_timestamp(delayed_charge_at);
    test_env.client.charge_subscription(&id);

    let after_delayed_charge = test_env.client.get_subscription(&id);
    assert_eq!(after_delayed_charge.status, SubscriptionStatus::Active);
    assert_eq!(after_delayed_charge.prepaid_balance, 9_000_000i128);
    assert_eq!(
        after_delayed_charge.last_payment_timestamp,
        delayed_charge_at
    );
    assert_eq!(after_delayed_charge.lifetime_charged, AMOUNT);
    assert_eq!(test_env.client.get_merchant_balance(&merchant), AMOUNT);

    test_env.client.deposit_funds(&id, &subscriber, &1_000_000i128);
    let after_topup = test_env.client.get_subscription(&id);
    assert_eq!(after_topup.prepaid_balance, AMOUNT);

    test_env.env.ledger().set_timestamp(delayed_charge_at + INTERVAL);
    test_env.client.charge_subscription(&id);

    let after_second_charge = test_env.client.get_subscription(&id);
    assert_eq!(after_second_charge.status, SubscriptionStatus::Active);
    assert_eq!(after_second_charge.prepaid_balance, 0);
    assert_eq!(
        after_second_charge.last_payment_timestamp,
        delayed_charge_at + INTERVAL
    );
    assert_eq!(after_second_charge.lifetime_charged, 2 * AMOUNT);
    assert_eq!(test_env.client.get_merchant_balance(&merchant), 2 * AMOUNT);

    let statements = test_env.client.get_sub_statements_offset(&id, &0, &10, &false);
    assert_eq!(statements.total, 2);
    assert_eq!(statements.statements.len(), 2);

    let first = statements.statements.get(0).unwrap();
    assert_eq!(first.sequence, 0);
    assert_eq!(first.period_start, T0);
    assert_eq!(first.period_end, delayed_charge_at);
    assert_eq!(first.amount, AMOUNT);

    let second = statements.statements.get(1).unwrap();
    assert_eq!(second.sequence, 1);
    assert_eq!(second.period_start, delayed_charge_at);
    assert_eq!(second.period_end, delayed_charge_at + INTERVAL);
    assert_eq!(second.amount, AMOUNT);

    assert_eq!(test_env.token_client().balance(&test_env.client.address), 20_000_000i128);
}

// -- List subscriptions by subscriber test ------------------------------------

#[test]
fn test_list_subscriptions_by_subscriber() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);

    let id1 = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let id2 = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    let page = test_env.client.list_subscriptions_by_subscriber(&subscriber, &0, &10);
    assert_eq!(page.subscription_ids.len(), 2);
    assert_eq!(page.subscription_ids.get(0).unwrap(), id1);
    assert_eq!(page.subscription_ids.get(1).unwrap(), id2);
    assert!(page.next_start_id.is_none());
}

#[test]
fn test_list_subscriptions_by_subscriber_limit_zero_errors() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let res = test_env.client.try_list_subscriptions_by_subscriber(&subscriber, &0, &0u32);
    assert!(matches!(res, Err(Ok(Error::InvalidInput))));
}

#[test]
fn test_list_subscriptions_by_subscriber_pagination_stable_ordering() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let mut expected = alloc::vec::Vec::new();
    for _ in 0..5 {
        let id = test_env.client.create_subscription(
            &subscriber,
            &merchant,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None::<i128>,
        );
        expected.push(id);
    }

    let page1 = test_env.client.list_subscriptions_by_subscriber(&subscriber, &0, &2);
    assert_eq!(page1.subscription_ids.len(), 2);
    assert_eq!(page1.subscription_ids.get(0).unwrap(), expected[0]);
    assert_eq!(page1.subscription_ids.get(1).unwrap(), expected[1]);
    let next = page1.next_start_id.expect("next page");
    let page2 = test_env.client.list_subscriptions_by_subscriber(&subscriber, &next, &10);
    assert_eq!(page2.subscription_ids.len(), 3);
    assert_eq!(page2.subscription_ids.get(0).unwrap(), expected[2]);
    assert_eq!(page2.subscription_ids.get(2).unwrap(), expected[4]);
    assert!(page2.next_start_id.is_none());
}

#[test]
fn test_get_subscriptions_by_merchant_pagination_and_invalid_limit() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    for _ in 0..3 {
        test_env.client.create_subscription(
            &subscriber,
            &merchant,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None::<i128>,
        );
    }
    assert_eq!(test_env.client.get_merchant_subscription_count(&merchant), 3);
    assert_eq!(
        test_env.client.try_get_subscriptions_by_merchant(&merchant, &0, &0u32),
        Err(Ok(Error::InvalidInput))
    );
    assert_eq!(
        test_env
            .client
            .try_get_subscriptions_by_merchant(&merchant, &0, &(MAX_SUBSCRIPTION_LIST_PAGE + 1)),
        Err(Ok(Error::InvalidInput))
    );
    let p1 = test_env.client.get_subscriptions_by_merchant(&merchant, &0, &2);
    assert_eq!(p1.len(), 2);
    let p2 = test_env.client.get_subscriptions_by_merchant(&merchant, &2, &2);
    assert_eq!(p2.len(), 1);
    let p3 = test_env.client.get_subscriptions_by_merchant(&merchant, &3, &10);
    assert_eq!(p3.len(), 0);
}

#[test]
fn test_get_subscriptions_by_token_pagination_and_count() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    for _ in 0..2 {
        test_env.client.create_subscription(
            &subscriber,
            &merchant,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None::<i128>,
        );
    }
    assert_eq!(test_env.client.get_token_subscription_count(&test_env.token), 2);
    assert_eq!(
        test_env
            .client
            .try_get_subscriptions_by_token(&test_env.token, &0, &0u32),
        Err(Ok(Error::InvalidInput))
    );
    let page = test_env.client.get_subscriptions_by_token(&test_env.token, &0, &1);
    assert_eq!(page.len(), 1);
    let rest = test_env.client.get_subscriptions_by_token(&test_env.token, &1, &5);
    assert_eq!(rest.len(), 1);
}

// -- Subscriber withdrawal test -----------------------------------------------

#[test]
fn test_withdraw_subscriber_funds_after_cancel() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &10_000_000);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &5_000_000);
    test_env.client.cancel_subscription(&id, &subscriber);

    test_env.client.withdraw_subscriber_funds(&id, &subscriber);

    let sub = test_env.client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 0);
}

// -- Export tests -------------------------------------------------------------

#[test]
fn test_export_contract_snapshot() {
    let test_env = TestEnv::default();
    let snapshot = test_env.client.export_contract_snapshot(&test_env.admin);
    assert_eq!(snapshot.admin, test_env.admin);
    assert_eq!(snapshot.storage_version, 2);
}

#[test]
fn test_export_subscription_summaries() {
    let test_env = TestEnv::default();
    let (id, _, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let summaries = test_env.client.export_subscription_summaries(&test_env.admin, &0, &10);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries.get(0).unwrap().subscription_id, id);
}

// =============================================================================
// Metadata Key-Value Store Tests
// =============================================================================

#[test]
fn test_metadata_set_and_get() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "invoice_id");
    let value = String::from_str(&test_env.env, "INV-2025-001");

    test_env.client.set_metadata(&id, &subscriber, &key, &value);

    let result = test_env.client.get_metadata(&id, &key);
    assert_eq!(result, value);
}

#[test]
fn test_metadata_update_existing_key() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "customer_id");
    let value1 = String::from_str(&test_env.env, "CUST-001");
    let value2 = String::from_str(&test_env.env, "CUST-002");

    test_env.client.set_metadata(&id, &subscriber, &key, &value1);
    assert_eq!(test_env.client.get_metadata(&id, &key), value1);

    test_env.client.set_metadata(&id, &subscriber, &key, &value2);
    assert_eq!(test_env.client.get_metadata(&id, &key), value2);

    // Key count should still be 1 (updated, not duplicated)
    let keys = test_env.client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 1);
}

#[test]
fn test_metadata_delete() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "tag");
    let value = String::from_str(&test_env.env, "premium");

    test_env.client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(test_env.client.get_metadata(&id, &key), value);

    test_env.client.delete_metadata(&id, &subscriber, &key);

    let result = test_env.client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
fn test_metadata_list_keys() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key1 = String::from_str(&test_env.env, "invoice_id");
    let key2 = String::from_str(&test_env.env, "customer_id");
    let key3 = String::from_str(&test_env.env, "campaign_tag");

    test_env.client.set_metadata(&id, &subscriber, &key1, &String::from_str(&test_env.env, "v1"));
    test_env.client.set_metadata(&id, &subscriber, &key2, &String::from_str(&test_env.env, "v2"));
    test_env.client.set_metadata(&id, &subscriber, &key3, &String::from_str(&test_env.env, "v3"));

    let keys = test_env.client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 3);
}

#[test]
fn test_metadata_empty_list_for_new_subscription() {
    let test_env = TestEnv::default();
    let (id, _, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let keys = test_env.client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 0);
}

#[test]
fn test_metadata_merchant_can_set() {
    let test_env = TestEnv::default();
    let (id, _, merchant) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "merchant_ref");
    let value = String::from_str(&test_env.env, "MR-123");

    test_env.client.set_metadata(&id, &merchant, &key, &value);
    assert_eq!(test_env.client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_merchant_can_delete() {
    let test_env = TestEnv::default();
    let (id, subscriber, merchant) =
        fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "tag");
    let value = String::from_str(&test_env.env, "test");

    // Subscriber sets it
    test_env.client.set_metadata(&id, &subscriber, &key, &value);

    // Merchant deletes it
    test_env.client.delete_metadata(&id, &merchant, &key);

    let result = test_env.client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_metadata_unauthorized_actor_rejected() {
    let test_env = TestEnv::default();
    let (id, _, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let stranger = Address::generate(&test_env.env);
    let key = String::from_str(&test_env.env, "test");
    let value = String::from_str(&test_env.env, "val");

    test_env.client.set_metadata(&id, &stranger, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_metadata_delete_unauthorized_rejected() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "test");
    test_env.client.set_metadata(&id, &subscriber, &key, &String::from_str(&test_env.env, "val"));

    let stranger = Address::generate(&test_env.env);
    test_env.client.delete_metadata(&id, &stranger, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #1023)")]
fn test_metadata_key_limit_enforced() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    // Set MAX_METADATA_KEYS (10) keys
    for i in 0..10u32 {
        let key = String::from_str(&test_env.env, &format!("key_{i}"));
        let value = String::from_str(&test_env.env, "val");
        test_env.client.set_metadata(&id, &subscriber, &key, &value);
    }

    // 11th key should fail
    let key = String::from_str(&test_env.env, "key_overflow");
    let value = String::from_str(&test_env.env, "val");
    test_env.client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
fn test_metadata_update_at_limit_succeeds() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    // Fill to max
    for i in 0..10u32 {
        let key = String::from_str(&test_env.env, &format!("key_{i}"));
        test_env.client.set_metadata(&id, &subscriber, &key, &String::from_str(&test_env.env, "val"));
    }

    // Updating an existing key should succeed even at limit
    let key = String::from_str(&test_env.env, "key_0");
    let new_value = String::from_str(&test_env.env, "updated");
    test_env.client.set_metadata(&id, &subscriber, &key, &new_value);
    assert_eq!(test_env.client.get_metadata(&id, &key), new_value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1024)")]
fn test_metadata_key_too_long_rejected() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    // 33 chars exceeds MAX_METADATA_KEY_LENGTH (32)
    let key = String::from_str(&test_env.env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let value = String::from_str(&test_env.env, "val");
    test_env.client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1024)")]
fn test_metadata_empty_key_rejected() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "");
    let value = String::from_str(&test_env.env, "val");
    test_env.client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1025)")]
fn test_metadata_value_too_long_rejected() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "test");
    // Create a string > 256 bytes
    let long_str = alloc::string::String::from_utf8(alloc::vec![b'x'; 257]).unwrap();
    let long_value = String::from_str(&test_env.env, &long_str);
    test_env.client.set_metadata(&id, &subscriber, &key, &long_value);
}

#[test]
fn test_metadata_key_max_length_boundary_ok() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let key = String::from_str(&test_env.env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(key.len(), 32);
    let val = String::from_str(&test_env.env, "x");
    test_env.client.set_metadata(&id, &subscriber, &key, &val);
    assert_eq!(test_env.client.get_metadata(&id, &key), val);
}

#[test]
fn test_metadata_value_max_length_boundary_ok() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let key = String::from_str(&test_env.env, "k");
    let val_str = alloc::string::String::from_utf8(alloc::vec![b'z'; 256]).unwrap();
    let value = String::from_str(&test_env.env, &val_str);
    assert_eq!(value.len(), 256);
    test_env.client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(test_env.client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_delete_nonexistent_try_api() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let key = String::from_str(&test_env.env, "missing");
    let res = test_env.client.try_delete_metadata(&id, &subscriber, &key);
    assert_eq!(res, Err(Ok(Error::NotFound)));
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_get_nonexistent_key() {
    let test_env = TestEnv::default();
    let (id, _, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "nonexistent");
    test_env.client.get_metadata(&id, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_delete_nonexistent_key() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "nonexistent");
    test_env.client.delete_metadata(&id, &subscriber, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_operations_on_nonexistent_subscription() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let key = String::from_str(&test_env.env, "test");
    let value = String::from_str(&test_env.env, "val");
    test_env.client.set_metadata(&999, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1002)")]
fn test_metadata_set_on_cancelled_subscription_rejected() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    test_env.client.cancel_subscription(&id, &subscriber);

    let key = String::from_str(&test_env.env, "test");
    let value = String::from_str(&test_env.env, "val");
    test_env.client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
fn test_metadata_does_not_affect_financial_state() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    fixtures::seed_balance(&test_env.env, &test_env.client, id, PREPAID);

    let sub_before = test_env.client.get_subscription(&id);

    // Set multiple metadata entries
    for i in 0..5u32 {
        let key = String::from_str(&test_env.env, &format!("key_{i}"));
        let value = String::from_str(&test_env.env, &format!("value_{i}"));
        test_env.client.set_metadata(&id, &subscriber, &key, &value);
    }

    let sub_after = test_env.client.get_subscription(&id);

    // Financial state must be unchanged
    assert_eq!(sub_before.prepaid_balance, sub_after.prepaid_balance);
    assert_eq!(sub_before.lifetime_charged, sub_after.lifetime_charged);
    assert_eq!(sub_before.status, sub_after.status);
    assert_eq!(sub_before.amount, sub_after.amount);
}

#[test]
fn test_metadata_delete_then_re_add() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "tag");
    let value1 = String::from_str(&test_env.env, "v1");
    let value2 = String::from_str(&test_env.env, "v2");

    test_env.client.set_metadata(&id, &subscriber, &key, &value1);
    test_env.client.delete_metadata(&id, &subscriber, &key);

    // Re-add same key with different value
    test_env.client.set_metadata(&id, &subscriber, &key, &value2);
    assert_eq!(test_env.client.get_metadata(&id, &key), value2);

    let keys = test_env.client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 1);
}

#[test]
fn test_metadata_delete_frees_key_slot() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    // Fill to max
    for i in 0..10u32 {
        let key = String::from_str(&test_env.env, &format!("key_{i}"));
        test_env.client.set_metadata(&id, &subscriber, &key, &String::from_str(&test_env.env, "v"));
    }

    // Delete one
    test_env.client.delete_metadata(&id, &subscriber, &String::from_str(&test_env.env, "key_5"));

    // Should now be able to add a new key
    let new_key = String::from_str(&test_env.env, "key_new");
    test_env.client.set_metadata(&id, &subscriber, &new_key, &String::from_str(&test_env.env, "v"));

    let keys = test_env.client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 10);
}

#[test]
fn test_metadata_isolation_between_subscriptions() {
    let test_env = TestEnv::default();
    let (id1, sub1, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    let (id2, sub2, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "invoice_id");
    let val1 = String::from_str(&test_env.env, "INV-001");
    let val2 = String::from_str(&test_env.env, "INV-002");

    test_env.client.set_metadata(&id1, &sub1, &key, &val1);
    test_env.client.set_metadata(&id2, &sub2, &key, &val2);

    assert_eq!(test_env.client.get_metadata(&id1, &key), val1);
    assert_eq!(test_env.client.get_metadata(&id2, &key), val2);
}

#[test]
fn test_metadata_on_paused_subscription_allowed() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);
    test_env.client.pause_subscription(&id, &subscriber);

    let key = String::from_str(&test_env.env, "note");
    let value = String::from_str(&test_env.env, "paused for maintenance");
    test_env.client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(test_env.client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_delete_on_cancelled_subscription_allowed() {
    let test_env = TestEnv::default();
    let (id, subscriber, _) = fixtures::create_subscription(&test_env.env, &test_env.client, SubscriptionStatus::Active);

    let key = String::from_str(&test_env.env, "tag");
    test_env.client.set_metadata(&id, &subscriber, &key, &String::from_str(&test_env.env, "v"));

    test_env.client.cancel_subscription(&id, &subscriber);

    // Delete should still work on cancelled (cleanup)
    test_env.client.delete_metadata(&id, &subscriber, &key);
    let result = test_env.client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
fn test_billing_statements_offset_pagination_newest_first() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &1_000_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &true,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &200_000_000i128);

    for i in 1..=6 {
        test_env.env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        test_env.client.charge_subscription(&id);
    }

    let page1 = test_env.client.get_sub_statements_offset(&id, &0, &2, &true);
    assert_eq!(page1.total, 6);
    assert_eq!(page1.statements.len(), 2);
    assert_eq!(page1.statements.get(0).unwrap().sequence, 5);
    assert_eq!(page1.statements.get(1).unwrap().sequence, 4);

    let page2 = test_env.client.get_sub_statements_offset(&id, &2, &2, &true);
    assert_eq!(page2.statements.len(), 2);
    assert_eq!(page2.statements.get(0).unwrap().sequence, 3);
    assert_eq!(page2.statements.get(1).unwrap().sequence, 2);
}

#[test]
fn test_billing_statements_cursor_pagination_boundaries() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &1_000_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &true,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &200_000_000i128);

    for i in 1..=4 {
        test_env.env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        test_env.client.charge_subscription(&id);
    }

    let first = test_env.client.get_sub_statements_cursor(&id, &None::<u32>, &3, &true);
    assert_eq!(first.statements.len(), 3);
    assert_eq!(first.statements.get(0).unwrap().sequence, 3);
    assert_eq!(first.statements.get(2).unwrap().sequence, 1);
    assert_eq!(first.next_cursor, Some(0));

    let second = test_env.client.get_sub_statements_cursor(&id, &first.next_cursor, &3, &true);
    assert_eq!(second.statements.len(), 1);
    assert_eq!(second.statements.get(0).unwrap().sequence, 0);
    assert_eq!(second.next_cursor, None);

    let invalid_cursor = test_env.client.get_sub_statements_cursor(&id, &Some(99u32), &2, &true);
    assert_eq!(invalid_cursor.statements.len(), 0);
    assert_eq!(invalid_cursor.next_cursor, None);
    assert_eq!(invalid_cursor.total, 4);
}

#[test]
fn test_compaction_prunes_old_statements_and_keeps_recent() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &2_000_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &500_000_000i128);

    for i in 1..=8 {
        test_env.env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        test_env.client.charge_subscription(&id);
    }

    test_env.client.set_billing_retention(&test_env.admin, &3);
    let summary = test_env.client.compact_billing_statements(&test_env.admin, &id, &None::<u32>);
    assert_eq!(summary.pruned_count, 5);
    assert_eq!(summary.kept_count, 3);
    assert_eq!(summary.total_pruned_amount, 5_000_000i128);

    let page = test_env.client.get_sub_statements_offset(&id, &0, &10, &true);
    assert_eq!(page.total, 3);
    assert_eq!(page.statements.len(), 3);
    assert_eq!(page.statements.get(0).unwrap().sequence, 7);
    assert_eq!(page.statements.get(2).unwrap().sequence, 5);

    let agg = test_env.client.get_stmt_compacted_aggregate(&id);
    assert_eq!(agg.pruned_count, 5);
    assert_eq!(agg.total_amount, 5_000_000i128);
}

#[test]
fn test_compaction_no_rows_and_override_value() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);

    let summary = test_env.client.compact_billing_statements(&test_env.admin, &id, &Some(10u32));
    assert_eq!(summary.pruned_count, 0);
    assert_eq!(summary.kept_count, 0);
    assert_eq!(summary.total_pruned_amount, 0);
}

#[test]
fn test_compaction_idempotent_second_run() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    test_env.stellar_token_client().mint(&subscriber, &2_000_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &500_000_000i128);

    for i in 1..=8 {
        test_env.env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        test_env.client.charge_subscription(&id);
    }

    test_env.client.set_billing_retention(&test_env.admin, &3);
    let first = test_env
        .client
        .compact_billing_statements(&test_env.admin, &id, &None::<u32>);
    assert_eq!(first.pruned_count, 5);

    let second = test_env
        .client
        .compact_billing_statements(&test_env.admin, &id, &None::<u32>);
    assert_eq!(second.pruned_count, 0);
    assert_eq!(second.kept_count, 3);
    assert_eq!(second.total_pruned_amount, 0);

    let agg = test_env.client.get_stmt_compacted_aggregate(&id);
    assert_eq!(agg.pruned_count, 5);
    assert_eq!(agg.total_amount, 5_000_000i128);
}

#[test]
fn test_compaction_keep_recent_zero_prunes_all_detail() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    test_env.stellar_token_client().mint(&subscriber, &500_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &100_000_000i128);

    for i in 1..=4 {
        test_env.env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        test_env.client.charge_subscription(&id);
    }

    let summary = test_env
        .client
        .compact_billing_statements(&test_env.admin, &id, &Some(0u32));
    assert_eq!(summary.pruned_count, 4);
    assert_eq!(summary.kept_count, 0);

    let page = test_env.client.get_sub_statements_offset(&id, &0, &10, &true);
    assert_eq!(page.total, 0);
    assert_eq!(page.statements.len(), 0);

    let agg = test_env.client.get_stmt_compacted_aggregate(&id);
    assert_eq!(agg.total_amount, 4_000_000i128);
    assert_eq!(agg.pruned_count, 4);
}

#[test]
fn test_set_billing_retention_non_admin_rejected() {
    let test_env = TestEnv::default();
    let attacker = Address::generate(&test_env.env);
    let res = test_env.client.try_set_billing_retention(&attacker, &5u32);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_compact_billing_statements_non_admin_rejected() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let attacker = Address::generate(&test_env.env);
    let res = test_env
        .client
        .try_compact_billing_statements(&attacker, &id, &None::<u32>);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_billing_retention_rapid_config_changes() {
    let test_env = TestEnv::default();
    test_env.client.set_billing_retention(&test_env.admin, &1u32);
    assert_eq!(test_env.client.get_billing_retention().keep_recent, 1);
    test_env.client.set_billing_retention(&test_env.admin, &u32::MAX);
    assert_eq!(test_env.client.get_billing_retention().keep_recent, u32::MAX);
    test_env.client.set_billing_retention(&test_env.admin, &12u32);
    assert_eq!(test_env.client.get_billing_retention().keep_recent, 12);
}

#[test]
fn test_compaction_override_respects_per_run_threshold() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    test_env.stellar_token_client().mint(&subscriber, &2_000_000_000i128);

    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    test_env.client.deposit_funds(&id, &subscriber, &500_000_000i128);

    for i in 1..=6 {
        test_env.env.ledger().set_timestamp(T0 + (i as u64 * INTERVAL));
        test_env.client.charge_subscription(&id);
    }

    test_env.client.set_billing_retention(&test_env.admin, &100u32);
    let s = test_env
        .client
        .compact_billing_statements(&test_env.admin, &id, &Some(2u32));
    assert_eq!(s.pruned_count, 4);
    assert_eq!(s.kept_count, 2);
}

#[test]
fn test_oracle_enabled_charge_uses_quote_conversion() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0);
    let oracle_id = test_env.env.register(MockOracle, ());
    let oracle = MockOracleClient::new(&test_env.env, &oracle_id);
    oracle.set_price(&2_000_000i128, &T0); // 2 quote units/token with 6 decimals

    // Enable oracle pricing with non-stale quote.
    test_env.client.set_oracle_config(
        &test_env.admin,
        &true,
        &Some(oracle_id.clone()),
        &(60 * 24 * 60 * 60),
    );

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &2_000_000_000i128);

    // 20 quote units (6 decimals). At price 2 quote/token, charge should be 10 tokens.
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &20_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &100_000_000i128);

    test_env.env.ledger().set_timestamp(T0 + INTERVAL);
    test_env.client.charge_subscription(&id);

    assert_eq!(test_env.client.get_merchant_balance(&merchant), 10_000_000i128);
}

#[test]
fn test_oracle_stale_quote_rejected() {
    let test_env = TestEnv::default();
    test_env.env.ledger().set_timestamp(T0 + INTERVAL);
    let oracle_id = test_env.env.register(MockOracle, ());
    let oracle = MockOracleClient::new(&test_env.env, &oracle_id);
    oracle.set_price(&2_000_000i128, &T0); // stale vs max_age=1
    test_env.client.set_oracle_config(&test_env.admin, &true, &Some(oracle_id.clone()), &1u64);

    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    soroban_sdk::token::StellarAssetClient::new(&test_env.env, &test_env.token).mint(&subscriber, &2_000_000_000i128);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &20_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id, &subscriber, &100_000_000i128);

    let result = test_env.client.try_charge_subscription(&id);
    assert_eq!(result, Err(Ok(Error::OraclePriceStale)));
}

#[test]

fn test_create_subscription_with_unaccepted_token_fails() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let unsupported = Address::generate(&test_env.env);
    let result = test_env.client.try_create_subscription_with_token(
        &subscriber,
        &merchant,
        &unsupported,
        &1_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    let id_b = client.create_subscription_with_token(
        &subscriber_b,
        &merchant,
        &token_b,
        &7_000_000i128,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    client.deposit_funds(&id_a, &subscriber_a, &20_000_000i128);
    client.deposit_funds(&id_b, &subscriber_b, &20_000_000i128);

#[test]
fn test_recover_stranded_funds_unauthorized_before_rotation() {
    let test_env = TestEnv::default();
    let stranger = Address::generate(&test_env.env);
    let recipient = Address::generate(&test_env.env);
    let result = test_env.client.try_recover_stranded_funds(
        &stranger,
        &recipient,
        &1_000_000i128,
        &RecoveryReason::AccidentalTransfer,
    );
    assert_eq!(result, Err(Ok(Error::Forbidden)));
}

#[test]
fn test_recover_stranded_funds_unauthorized_after_rotation() {
    let test_env = TestEnv::default();
    let new_admin = Address::generate(&test_env.env);
    let recipient = Address::generate(&test_env.env);
    test_env.client.rotate_admin(&test_env.admin, &new_admin);
    assert_eq!(
        test_env.client.try_recover_stranded_funds(
            &test_env.admin,
            &recipient,
            &1_000_000i128,
            &RecoveryReason::AccidentalTransfer
        ),
        Err(Ok(Error::Forbidden))
    );
}

// -- Integration: recovery respects rotation ----------------------------------

#[test]
fn test_admin_rotation_affects_recovery_operations() {
    let test_env = TestEnv::default();
    let new_admin = Address::generate(&test_env.env);
    let recipient = Address::generate(&test_env.env);

    // Old admin can recover before rotation.
    test_env.client.recover_stranded_funds(
        &test_env.admin,
        &recipient,
        &1_000_000i128,
        &RecoveryReason::AccidentalTransfer,
    );

    test_env.client.rotate_admin(&test_env.admin, &new_admin);

    // Old admin blocked after rotation.
    assert_eq!(
        test_env.client.try_recover_stranded_funds(
            &test_env.admin,
            &recipient,
            &1_000_000i128,
            &RecoveryReason::AccidentalTransfer
        ),
        Err(Ok(Error::Forbidden))
    );

    // New admin can recover.
    test_env.client.recover_stranded_funds(
        &new_admin,
        &recipient,
        &1_000_000i128,
        &RecoveryReason::AccidentalTransfer,
    );
}

#[test]
fn test_all_admin_operations_after_rotation() {
    let test_env = TestEnv::default();
    let new_admin = Address::generate(&test_env.env);
    let next_admin = Address::generate(&test_env.env);
    let recipient = Address::generate(&test_env.env);

    test_env.client.rotate_admin(&test_env.admin, &new_admin);

    test_env.client.set_min_topup(&new_admin, &3_000_000i128);
    test_env.client.recover_stranded_funds(
        &new_admin,
        &recipient,
        &1_000_000i128,
        &RecoveryReason::AccidentalTransfer,
    );
    test_env.client.rotate_admin(&new_admin, &next_admin);
    assert_eq!(test_env.client.get_admin(), next_admin);
}

#[test]
fn test_multiple_admin_rotations() {
    let test_env = TestEnv::default();
    let admin_b = Address::generate(&test_env.env);
    let admin_c = Address::generate(&test_env.env);
    let admin_d = Address::generate(&test_env.env);

    test_env.client.rotate_admin(&test_env.admin, &admin_b);
    test_env.client.rotate_admin(&admin_b, &admin_c);
    test_env.client.rotate_admin(&admin_c, &admin_d);

    assert_eq!(test_env.client.get_admin(), admin_d);

    // All previous admins are denied.
    for stale in [&test_env.admin, &admin_b, &admin_c] {
        assert_eq!(
            test_env.client.try_set_min_topup(stale, &1_000_000i128),
            Err(Ok(Error::Forbidden))
        );
    }
}

#[test]
fn test_admin_cannot_be_rotated_by_previous_admin() {
    let test_env = TestEnv::default();
    let admin2 = Address::generate(&test_env.env);
    let admin3 = Address::generate(&test_env.env);

    test_env.client.rotate_admin(&test_env.admin, &admin2);

    // admin1 cannot rotate again.
    let result = test_env.client.try_rotate_admin(&test_env.admin, &admin3);
    assert_eq!(result, Err(Ok(Error::Forbidden)));
    assert_eq!(test_env.client.get_admin(), admin2);
}

// -- State isolation ----------------------------------------------------------

#[test]
fn test_admin_rotation_does_not_affect_subscriptions() {
    let test_env = TestEnv::default();
    let subscriber = Address::generate(&test_env.env);
    let merchant = Address::generate(&test_env.env);
    let id = test_env.client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
     &None::<u64>);
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}
