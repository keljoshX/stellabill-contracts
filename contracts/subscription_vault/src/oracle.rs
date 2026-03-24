//! Optional oracle integration for cross-currency pricing.

use crate::types::{Error, OracleConfig, OraclePrice, Subscription};
use soroban_sdk::{Address, Env, Symbol, Vec};

const KEY_ORACLE_ENABLED: &str = "oracle_enabled";
const KEY_ORACLE_ADDR: &str = "oracle_addr";
const KEY_ORACLE_MAX_AGE: &str = "oracle_max_age";

pub fn set_oracle_config(
    env: &Env,
    enabled: bool,
    oracle: Option<Address>,
    max_age_seconds: u64,
) -> Result<(), Error> {
    #[cfg(not(feature = "oracle-pricing"))]
    {
        let _ = (env, enabled, oracle, max_age_seconds);
        return Err(Error::InvalidInput);
    }
    #[cfg(feature = "oracle-pricing")]
    {
        if enabled {
            if oracle.is_none() {
                return Err(Error::OracleNotConfigured);
            }
            if max_age_seconds == 0 {
                return Err(Error::InvalidInput);
            }
        }
        let storage = env.storage().instance();
        storage.set(&Symbol::new(env, KEY_ORACLE_ENABLED), &enabled);
        if let Some(ref addr) = oracle {
            storage.set(&Symbol::new(env, KEY_ORACLE_ADDR), addr);
        } else {
            storage.remove(&Symbol::new(env, KEY_ORACLE_ADDR));
        }
        storage.set(&Symbol::new(env, KEY_ORACLE_MAX_AGE), &max_age_seconds);
        Ok(())
    }
}

pub fn get_oracle_config(env: &Env) -> OracleConfig {
    #[cfg(not(feature = "oracle-pricing"))]
    {
        let _ = env;
        return OracleConfig {
            enabled: false,
            oracle: None,
            max_age_seconds: 0,
        };
    }
    #[cfg(feature = "oracle-pricing")]
    {
        let storage = env.storage().instance();
        OracleConfig {
            enabled: storage
                .get(&Symbol::new(env, KEY_ORACLE_ENABLED))
                .unwrap_or(false),
            oracle: storage.get::<_, Address>(&Symbol::new(env, KEY_ORACLE_ADDR)),
            max_age_seconds: storage
                .get(&Symbol::new(env, KEY_ORACLE_MAX_AGE))
                .unwrap_or(0u64),
        }
    }
}

/// Resolve token-denominated charge amount.
///
/// With oracle disabled, returns `subscription.amount` as-is.
/// With oracle enabled, interprets `subscription.amount` as quote units and converts
/// to token base units using oracle quote:
///
/// token_amount = ceil(quote_amount * 10^token_decimals / quote_per_token)
pub fn resolve_charge_amount(env: &Env, subscription: &Subscription) -> Result<i128, Error> {
    #[cfg(not(feature = "oracle-pricing"))]
    {
        let _ = env;
        return Ok(subscription.amount);
    }
    #[cfg(feature = "oracle-pricing")]
    {
        let cfg = get_oracle_config(env);
        if !cfg.enabled {
            return Ok(subscription.amount);
        }

        let oracle = cfg.oracle.ok_or(Error::OracleNotConfigured)?;
        let price: OraclePrice =
            env.invoke_contract(&oracle, &Symbol::new(env, "latest_price"), Vec::new(env));

        if price.price <= 0 {
            return Err(Error::OraclePriceInvalid);
        }
        if price.timestamp == 0 {
            return Err(Error::OraclePriceUnavailable);
        }
        if cfg.max_age_seconds > 0 {
            let now = env.ledger().timestamp();
            if now.saturating_sub(price.timestamp) > cfg.max_age_seconds {
                return Err(Error::OraclePriceStale);
            }
        }

        let token_decimals =
            crate::admin::get_token_decimals(env, &subscription.token).unwrap_or(6);

        let scale = 10i128.checked_pow(token_decimals).ok_or(Error::Overflow)?;
        let numerator = subscription
            .amount
            .checked_mul(scale)
            .ok_or(Error::Overflow)?;
        let ceil_adjust = price.price.checked_sub(1).ok_or(Error::Overflow)?;
        let token_amount = numerator
            .checked_add(ceil_adjust)
            .ok_or(Error::Overflow)?
            .checked_div(price.price)
            .ok_or(Error::OraclePriceInvalid)?;

        if token_amount <= 0 {
            return Err(Error::OraclePriceInvalid);
        }
        Ok(token_amount)
    }
}
