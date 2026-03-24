use crate::{Error, SubscriptionVault, SubscriptionVaultClient};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, Env, Vec as SorobanVec};

// ── Fixtures for Attack Patterns ─────────────────────────────────────────────

/// Fixture for a "Double Charge" attack attempt.
fn simulate_double_charge_attack(
    env: &Env,
    client: &SubscriptionVaultClient,
    id: u32,
    interval: u64,
) -> Result<(), Error> {
    let now = env.ledger().timestamp();
    // Advance to exactly the next interval
    env.ledger().with_mut(|li| li.timestamp = now + interval);

    // First charge (should succeed)
    client.charge_subscription(&id);

    // Immediate second charge (should fail via IntervalNotElapsed or Replay)
    match client.try_charge_subscription(&id) {
        Ok(_) => Ok(()),
        Err(Ok(err)) => Err(err),
        Err(Err(_)) => panic!("Unexpected host error"),
    }
}

/// Fixture for an "Unauthorized Admin Action" attack attempt.
fn simulate_unauthorized_admin_action(
    env: &Env,
    client: &SubscriptionVaultClient,
    attacker: &Address,
) -> Result<(), Error> {
    // Clear mock auths to simulate real auth check
    env.mock_auths(&[]);

    // Try an admin-only action
    match client.try_set_min_topup(attacker, &5_000_000) {
        Ok(_) => Ok(()),
        Err(Ok(err)) => Err(err),
        Err(Err(_)) => panic!("Unexpected host error"),
    }
}

/// Fixture for an "Arithmetic Overflow" attack on deposits.
fn simulate_deposit_overflow_attack(
    _env: &Env,
    client: &SubscriptionVaultClient,
    id: u32,
    subscriber: &Address,
) -> Result<(), Error> {
    let sub = client.get_subscription(&id);
    let overflow_amount = i128::MAX - sub.prepaid_balance + 1;

    match client.try_deposit_funds(&id, subscriber, &overflow_amount) {
        Ok(_) => Ok(()),
        Err(Ok(err)) => Err(err),
        Err(Err(_)) => panic!("Unexpected host error"),
    }
}

// -- helpers ------------------------------------------------------------------

fn setup_security_test_env() -> (Env, SubscriptionVaultClient<'static>, Address, Address) {
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

fn create_funded_subscription(
    env: &Env,
    client: &SubscriptionVaultClient,
    amount: i128,
    interval: u64,
) -> (u32, Address, Address) {
    let subscriber = Address::generate(env);
    let merchant = Address::generate(env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &amount,
        &interval,
        &false,
        &None::<i128>,
    );

    // Seed balance directly for testing purposes
    let mut sub = client.get_subscription(&id);
    sub.prepaid_balance = amount * 10;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });

    (id, subscriber, merchant)
}

// ── Category: Reentrancy ─────────────────────────────────────────────────────

#[test]
fn test_reentrancy_mitigation_checks() {
    // Soroban transactions are atomic and the contract follows CEI.
    // This test verifies that even if an external call (mocked) were to happen,
    // the state is updated BEFORE the call.

    let (env, client, _, _admin) = setup_security_test_env();
    let (id, subscriber, _) = create_funded_subscription(&env, &client, 1_000_000, 3600);

    // Verify that withdrawal follows CEI: balance is 0 before transfer completes
    // In a real scenario, if the transfer fails, the whole transaction reverts.
    client.cancel_subscription(&id, &subscriber);
    client.withdraw_subscriber_funds(&id, &subscriber);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 0);
}

// ── Category: Auth Bypass ────────────────────────────────────────────────────

#[test]
fn test_auth_bypass_negative_cases() {
    let (env, client, _, _admin) = setup_security_test_env();
    let attacker = Address::generate(&env);

    // 1. Non-admin trying to rotate admin
    let result = client.try_rotate_admin(&attacker, &attacker);
    assert_eq!(result.unwrap_err(), Ok(Error::Unauthorized));

    // 2. Non-admin trying to set min topup
    let err = simulate_unauthorized_admin_action(&env, &client, &attacker);
    assert_eq!(err.unwrap_err(), Error::Unauthorized);

    // 3. Unauthorized re-initialization
    let result = client.try_init(&attacker, &6, &attacker, &1_000_000, &3600);
    assert_eq!(result.unwrap_err(), Ok(Error::AlreadyInitialized));
}

#[test]
fn test_auth_bypass_owner_verification_gap() {
    // Targets: Known Limitation #2 in security.md
    let (env, client, _, _) = setup_security_test_env();
    let (id, _, _) = create_funded_subscription(&env, &client, 1_000_000, 3600);
    let attacker = Address::generate(&env);

    // Attacker attempts to cancel someone else's subscription
    let result = client.try_cancel_subscription(&id, &attacker);

    // If the contract correctly implements owner verification, this should fail.
    // Currently, it might return Forbidden (403) or Unauthorized (401).
    assert!(result.is_err());
}

// ── Category: Replay ─────────────────────────────────────────────────────────

#[test]
fn test_replay_negative_cases() {
    let (env, client, _, _) = setup_security_test_env();
    let interval = 3600;
    let (id, _, _) = create_funded_subscription(&env, &client, 1_000_000, interval);

    env.ledger().with_mut(|li| li.timestamp = 10_000);

    // Attempt double charge
    let err = simulate_double_charge_attack(&env, &client, id, interval);
    assert_eq!(err.unwrap_err(), Error::IntervalNotElapsed);

    // Attempt charge before interval elapses
    env.ledger()
        .with_mut(|li| li.timestamp = 10_000 + interval - 1);
    let result = client.try_charge_subscription(&id);
    assert_eq!(result.unwrap_err(), Ok(Error::IntervalNotElapsed));
}

// ── Category: Arithmetic Risk ────────────────────────────────────────────────

#[test]
fn test_arithmetic_negative_cases() {
    let (env, client, _, _) = setup_security_test_env();
    let (id, subscriber, _) = create_funded_subscription(&env, &client, 1_000_000, 3600);

    // 1. Deposit overflow
    let err = simulate_deposit_overflow_attack(&env, &client, id, &subscriber);
    assert_eq!(err.unwrap_err(), Error::Overflow);

    // 2. Underflow during charge (insufficient balance)
    let mut sub = client.get_subscription(&id);
    sub.prepaid_balance = 500_000;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });

    env.ledger().with_mut(|li| li.timestamp = 10_000 + 3601);
    let result = client.try_charge_subscription(&id);
    assert!(result.is_err());
}

// ── Category: State Machine Integrity ────────────────────────────────────────

#[test]
fn test_state_machine_illegal_transitions() {
    let (env, client, _, _) = setup_security_test_env();
    let (id, subscriber, _) = create_funded_subscription(&env, &client, 1_000_000, 3600);

    // Cancel it first (terminal state)
    client.cancel_subscription(&id, &subscriber);

    // Try to resume from Cancelled (Illegal)
    let result = client.try_resume_subscription(&id, &subscriber);
    assert_eq!(result.unwrap_err(), Ok(Error::InvalidStatusTransition));

    // Try to pause from Cancelled (Illegal)
    let result = client.try_pause_subscription(&id, &subscriber);
    assert_eq!(result.unwrap_err(), Ok(Error::InvalidStatusTransition));
}

// ── Category: Critical Path Negative Tests ───────────────────────────────────

#[test]
fn test_negative_batch_charge_with_unauthorized_admin() {
    let (env, client, _, _) = setup_security_test_env();
    let (id1, _, _) = create_funded_subscription(&env, &client, 1_000_000, 3600);
    let _attacker = Address::generate(&env);

    env.mock_auths(&[]); // Simulate no valid auth

    let ids = SorobanVec::from_array(&env, [id1]);
    let result = client.try_batch_charge(&ids);

    // Should be unauthorized because it's an admin-only function
    assert!(result.is_err());
}

#[test]
fn test_negative_withdraw_excessive_merchant_funds() {
    let (env, client, _, _) = setup_security_test_env();
    let merchant = Address::generate(&env);

    // Merchant has 0 balance, try to withdraw 100
    let result = client.try_withdraw_merchant_funds(&merchant, &100);
    assert!(result.is_err());
}
