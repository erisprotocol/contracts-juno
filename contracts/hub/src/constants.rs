use cosmwasm_std::Decimal;

pub const CONTRACT_NAME: &str = "eris-hub";
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CONTRACT_DENOM: &str = "ujuno";
pub const CW20_PREFIX: &str = "juno1";
pub const REPLY_PARSE_COINS_RECEIVED: u64 = 2;

pub fn get_reward_fee_cap() -> Decimal {
    // 10% max reward fee
    Decimal::from_ratio(10_u128, 100_u128)
}
