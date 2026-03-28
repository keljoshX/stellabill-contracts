#![cfg(test)]

use crate::safe_math::{safe_add, safe_sub, safe_mul, safe_div, safe_pow};
use crate::types::Error;
use soroban_sdk::Env;

#[test]
fn test_safe_add_boundaries() {
    let _env = Env::default();
    
    // Normal cases
    assert_eq!(safe_add(10, 20).unwrap(), 30);
    assert_eq!(safe_add(-10, 20).unwrap(), 10);
    assert_eq!(safe_add(0, 0).unwrap(), 0);
    
    // Max boundary
    assert_eq!(safe_add(i128::MAX, 0).unwrap(), i128::MAX);
    assert_eq!(safe_add(i128::MAX, -1).unwrap(), i128::MAX - 1);
    
    // Overflow
    assert_eq!(safe_add(i128::MAX, 1), Err(Error::Overflow));
    assert_eq!(safe_add(i128::MAX, i128::MAX), Err(Error::Overflow));
    
    // Underflow (negative addition)
    assert_eq!(safe_add(i128::MIN, -1), Err(Error::Underflow));
}

#[test]
fn test_safe_sub_boundaries() {
    let _env = Env::default();
    
    // Normal cases
    assert_eq!(safe_sub(30, 10).unwrap(), 20);
    assert_eq!(safe_sub(10, 20).unwrap(), -10);
    
    // Min boundary
    assert_eq!(safe_sub(i128::MIN, 0).unwrap(), i128::MIN);
    assert_eq!(safe_sub(i128::MIN, -1).unwrap(), i128::MIN + 1);
    
    // Underflow
    assert_eq!(safe_sub(i128::MIN, 1), Err(Error::Underflow));
    assert_eq!(safe_sub(0, i128::MAX), Ok(-i128::MAX)); // Still safe
    assert_eq!(safe_sub(i128::MIN, i128::MAX), Err(Error::Underflow));
}

#[test]
fn test_safe_mul_boundaries() {
    let _env = Env::default();
    
    // Normal cases
    assert_eq!(safe_mul(10, 20).unwrap(), 200);
    assert_eq!(safe_mul(-10, 20).unwrap(), -200);
    assert_eq!(safe_mul(0, 100).unwrap(), 0);
    
    // Boundaries
    assert_eq!(safe_mul(i128::MAX, 1).unwrap(), i128::MAX);
    assert_eq!(safe_mul(i128::MIN, 1).unwrap(), i128::MIN);
    
    // Overflow
    assert_eq!(safe_mul(i128::MAX, 2), Err(Error::Overflow));
    assert_eq!(safe_mul(i128::MIN, 2), Err(Error::Underflow)); // MIN * 2 underflows
    assert_eq!(safe_mul(i128::MIN, -1), Err(Error::Overflow)); // MIN * -1 overflows (becomes MAX + 1)
}

#[test]
fn test_safe_div_boundaries() {
    let _env = Env::default();
    
    // Normal cases
    assert_eq!(safe_div(200, 10).unwrap(), 20);
    assert_eq!(safe_div(-200, 10).unwrap(), -20);
    
    // Division by zero
    assert_eq!(safe_div(100, 0), Err(Error::InvalidInput));
    
    // Minus one boundary
    assert_eq!(safe_div(i128::MIN, -1), Err(Error::Overflow));
}

#[test]
fn test_safe_pow_boundaries() {
    let _env = Env::default();
    
    // Normal cases
    assert_eq!(safe_pow(10, 2).unwrap(), 100);
    assert_eq!(safe_pow(2, 10).unwrap(), 1024);
    assert_eq!(safe_pow(10, 0).unwrap(), 1);
    assert_eq!(safe_pow(0, 10).unwrap(), 0);
    
    // Boundary bits (10^38 is ~2^126.2)
    assert_eq!(safe_pow(10, 38).unwrap(), 100_000_000_000_000_000_000_000_000_000_000_000_000);
    
    // Overflow
    assert_eq!(safe_pow(10, 39), Err(Error::Overflow));
    assert_eq!(safe_pow(2, 126).unwrap(), 85070591730234615865843651857942052864);
    assert_eq!(safe_pow(2, 127), Err(Error::Overflow));
}

#[test]
fn test_sequence_increment_regression() {
    // This simulates the counter logic we replaced in subscription.rs and statements.rs
    let next_id: u32 = u32::MAX - 1;
    let incremented = safe_add(next_id as i128, 1).unwrap() as u32;
    assert_eq!(incremented, u32::MAX);
    
    let over_max = safe_add(u32::MAX as i128, 1).unwrap() as u32;
    // Note: cast from i128 (u32::MAX + 1) to u32 will wrap in Rust 'as' cast,
    // but our logic ensures we stay within i128 until the very end.
    // In the contract, we use this for next_id which is u32.
    assert_eq!(over_max, 0); // (u32::MAX + 1) as u32 == 0
}
