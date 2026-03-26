# Merchant Withdrawals

Merchants can withdraw their accumulated balance from the Subscription Vault using
`withdraw_merchant_funds` for the default token bucket or
`withdraw_merchant_token_funds` for a specific accepted token bucket.
Funds accumulate to a merchant's balance each time a subscription for that merchant is
successfully charged.

## Process and Requirements

1. **Authorization**: The merchant must authorize the withdrawal transaction. The contract enforces this using `merchant.require_auth()`.
2. **Valid Amounts**: The `amount` to withdraw must be strictly positive (`> 0`). An attempt to withdraw `0` or a negative amount will result in `Error::InvalidAmount` (`405`).
3. **No Overdrafts**: A merchant cannot withdraw more than their currently accumulated balance. Overdraft attempts are rejected with `Error::InsufficientBalance` (`1003`).
4. **Zero Balance**: If a merchant has no recorded accumulated balance in the requested bucket
   (e.g., no subscriptions have been charged yet), withdrawal attempts will return
   `Error::NotFound` (`404`).
5. **Vault Solvency Check**: Before transfer, the contract verifies that its custody balance for
   the selected token is at least the requested withdrawal amount. If not, the withdrawal is
   rejected with `Error::InsufficientBalance` (`1003`) and the ledger is left unchanged.
6. **Token Isolation**: Merchant balances are stored per `(merchant, token)` bucket, so
   withdrawing one token never debits another token's earnings.

## Security Guarantees

- **Checks-Effects-Interactions**: The contract validates balances first, persists the debited
  merchant bucket, emits the withdrawal event, and only then performs the token transfer. If the
  external transfer fails, the invocation aborts atomically and storage rolls back.
- **Arithmetic Safety**: Internal checks comprehensively prevent overflows using checked arithmetic (`checked_add`, `checked_sub`).
- **No Side Effects**: A failed withdrawal (due to overdraft or mismatched auth) has no side-effects on the ledger state or other subscriptions.

## Interaction Flow
1. An admin charges a subscription using `charge_subscription`.
2. The `SubscriptionVault` increments the `merchant_balance` by the subscription's `amount`.
3. The merchant triggers `withdraw_merchant_funds` or `withdraw_merchant_token_funds`
   specifying the token bucket and amount to withdraw.
4. The contract debits only that merchant/token bucket and emits a withdrawal event with the
   token, amount, and remaining balance.
5. The requested token amount is transferred to the merchant's Stellar account.
