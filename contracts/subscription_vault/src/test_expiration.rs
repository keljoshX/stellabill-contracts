#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, Env, token};

fn setup_test_env() -> (Env, SubscriptionVaultClient<'static>, token::Client<'static>, token::StellarAssetClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1000);

    let admin = Address::generate(&env);
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_client = token::Client::new(&env, &token_id.address());
    let token_admin_client = token::StellarAssetClient::new(&env, &token_id.address());

    let min_topup = 1_000_000i128; // 1 USDC
    client.init(&token_id.address(), &6, &admin, &min_topup, &(7 * 24 * 60 * 60));

    (env, client, token_client, token_admin_client, admin)
}

#[test]
fn test_expiration_timing_and_charging() {
    let (env, client, token, token_admin, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let amount = 100i128;
    let interval = 10u64;
    let expires_at = 1050u64;

    let min_topup = 1_000_000i128;
    token_admin.mint(&subscriber, &(min_topup * 2));

    let sub_id = client.create_subscription_with_token(&subscriber, &merchant, &token.address, &amount, &interval, &false, &None::<i128>, &Some(expires_at, &None));

    client.deposit_funds(&sub_id, &subscriber, &min_topup);

    // Before expiry (timestamp 1010), should charge normally
    env.ledger().with_mut(|l| l.timestamp = 1010);
    client.charge_subscription(&sub_id);
    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.lifetime_charged, amount);

    // Exact expiry boundary (timestamp 1050)
    env.ledger().with_mut(|l| l.timestamp = 1050);
    let res = client.try_charge_subscription(&sub_id);
    assert!(res.is_err()); // Should reject
    
    // Status is technically rolled back to Active because of the error, 
    // but the expiration logic still prevents charging.
    let sub_expired = client.get_subscription(&sub_id);
    assert!(sub_expired.expires_at.is_some());

    // After expiry (timestamp 1060)
    env.ledger().with_mut(|l| l.timestamp = 1060);
    let res2 = client.try_charge_subscription(&sub_id);
    assert!(res2.is_err()); // Still rejects

    // Check withdrawal behavior after expiry
    let initial_balance = token.balance(&subscriber);
    client.withdraw_subscriber_funds(&sub_id, &subscriber);
    let final_balance = token.balance(&subscriber);
    assert!(final_balance > initial_balance);
}

#[test]
fn test_cleanup_and_archival() {
    let (env, client, token, token_admin, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let min_topup = 1_000_000i128;
    token_admin.mint(&subscriber, &(min_topup * 2));

    let sub_id = client.create_subscription_with_token(&subscriber, &merchant, &token.address, &100, &10, &false, &None::<i128>, &Some(1050, &None));

    client.deposit_funds(&sub_id, &subscriber, &min_topup);

    // Try cleanup before expiry/cancel - should fail
    let res = client.try_cleanup_subscription(&sub_id, &subscriber);
    assert!(res.is_err());

    // Expire it
    env.ledger().with_mut(|l| l.timestamp = 1050);
    
    // Cleanup now should succeed
    client.cleanup_subscription(&sub_id, &subscriber);
    
    let sub_archived = client.get_subscription(&sub_id);
    assert_eq!(sub_archived.status, SubscriptionStatus::Archived);

    // Archival reads - can still read it
    assert_eq!(sub_archived.amount, 100);

    // Ensure funds are not lost and can be withdrawn
    let initial_balance = token.balance(&subscriber);
    client.withdraw_subscriber_funds(&sub_id, &subscriber);
    assert_eq!(token.balance(&subscriber), initial_balance + min_topup);
}

#[test]
fn test_expiration_vs_cancellation() {
    let (env, client, token, token_admin, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Scenario 1: Cancel before expiry
    let sub_id1 = client.create_subscription_with_token(&subscriber, &merchant, &token.address, &100, &10, &false, &None::<i128>, &Some(1050, &None));
    
    client.cancel_subscription(&sub_id1, &subscriber);
    assert_eq!(client.get_subscription(&sub_id1).status, SubscriptionStatus::Cancelled);
    
    env.ledger().with_mut(|l| l.timestamp = 1060);
    // Should stay cancelled
    assert_eq!(client.get_subscription(&sub_id1).status, SubscriptionStatus::Cancelled);
    // Can be archived from Cancelled
    client.cleanup_subscription(&sub_id1, &subscriber);
    assert_eq!(client.get_subscription(&sub_id1).status, SubscriptionStatus::Archived);

    // Scenario 2: Expire without cancel
    let sub_id2 = client.create_subscription_with_token(&subscriber, &merchant, &token.address, &100, &10, &false, &None::<i128>, &Some(1050, &None));
    
    // Trigger expiration
    env.ledger().with_mut(|l| l.timestamp = 1060);
    let res = client.try_cancel_subscription(&sub_id2, &subscriber);
    assert!(res.is_err()); // Cannot cancel an expired subscription directly, it is already expired

    // Archiving should work
    client.cleanup_subscription(&sub_id2, &subscriber);
    assert_eq!(client.get_subscription(&sub_id2).status, SubscriptionStatus::Archived);
}

#[test]
fn test_deposit_rejected_when_expired() {
    let (env, client, token, token_admin, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    token_admin.mint(&subscriber, &1000);

    let sub_id = client.create_subscription_with_token(&subscriber, &merchant, &token.address, &100, &10, &false, &None::<i128>, &Some(1050, &None));

    env.ledger().with_mut(|l| l.timestamp = 1050);
    let res = client.try_deposit_funds(&sub_id, &subscriber, &100);
    assert!(res.is_err());
}
