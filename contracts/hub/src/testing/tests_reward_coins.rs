use std::vec;

use cosmwasm_std::testing::{mock_env, mock_info, MockApi, MockStorage, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{
    coin, to_binary, Addr, Coin, CosmosMsg, Decimal, DistributionMsg, Event, OwnedDeps, Reply,
    ReplyOn, StdResult, SubMsg, SubMsgResponse, Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use eris_staking::DecimalCheckedOps;

use eris_staking::hub::{
    CallbackMsg, ConfigResponse, ExecuteMsg, FeeConfig, InstantiateMsg, PendingBatch, QueryMsg,
    StateResponse,
};

use crate::constants::CONTRACT_DENOM;
use crate::contract::{execute, instantiate, reply};
use crate::state::State;
use crate::types::{Delegation, SendFee};

use super::custom_querier::CustomQuerier;
use super::helpers::{mock_dependencies, mock_env_at_timestamp, query_helper};

const COIN_CW20: &str = "juno1cw20";
const COIN_IBC: &str = "ibc/test";
const COIN_UTOKEN: &str = CONTRACT_DENOM;

//--------------------------------------------------------------------------------------------------
// Test setup
//--------------------------------------------------------------------------------------------------

fn setup_test() -> OwnedDeps<MockStorage, MockApi, CustomQuerier> {
    let mut deps = mock_dependencies();

    let res = instantiate(
        deps.as_mut(),
        mock_env_at_timestamp(10000),
        mock_info("deployer", &[]),
        InstantiateMsg {
            cw20_code_id: 69420,
            owner: "owner".to_string(),
            name: "Stake Token".to_string(),
            symbol: "STAKE".to_string(),
            decimals: 6,
            epoch_period: 259200,   // 3 * 24 * 60 * 60 = 3 days
            unbond_period: 1814400, // 21 * 24 * 60 * 60 = 21 days
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            protocol_fee_contract: "fee".to_string(),
            protocol_reward_fee: Decimal::from_ratio(1u128, 100u128),
            reward_coins: Some(vec![
                COIN_UTOKEN.to_string(),
                COIN_IBC.to_string(),
                COIN_CW20.to_string(),
            ]),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: Some("owner".to_string()),
                code_id: 69420,
                msg: to_binary(&Cw20InstantiateMsg {
                    name: "Stake Token".to_string(),
                    symbol: "STAKE".to_string(),
                    decimals: 6,
                    initial_balances: vec![],
                    mint: Some(MinterResponse {
                        minter: MOCK_CONTRACT_ADDR.to_string(),
                        cap: None
                    }),
                    marketing: None,
                })
                .unwrap(),
                funds: vec![],
                label: "Eris Liquid Staking Token".to_string(),
            }),
            1
        )
    );

    let event = Event::new("instantiate")
        .add_attribute("creator", MOCK_CONTRACT_ADDR)
        .add_attribute("admin", "admin")
        .add_attribute("code_id", "69420")
        .add_attribute("_contract_address", "stake_token");

    let res = reply(
        deps.as_mut(),
        mock_env_at_timestamp(10000),
        Reply {
            id: 1,
            result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
                events: vec![event],
                data: None,
            }),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    deps.querier.set_cw20_total_supply("stake_token", 0);
    deps
}

//--------------------------------------------------------------------------------------------------
// Execution
//--------------------------------------------------------------------------------------------------

#[test]
fn proper_instantiation() {
    let deps = setup_test();

    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(
        res,
        ConfigResponse {
            owner: "owner".to_string(),
            new_owner: None,
            stake_token: "stake_token".to_string(),
            epoch_period: 259200,
            unbond_period: 1814400,
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            fee_config: FeeConfig {
                protocol_fee_contract: Addr::unchecked("fee"),
                protocol_reward_fee: Decimal::from_ratio(1u128, 100u128)
            },
            reward_coins: vec![
                COIN_UTOKEN.to_string(),
                COIN_IBC.to_string(),
                COIN_CW20.to_string(),
            ]
        }
    );

    let res: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(
        res,
        StateResponse {
            total_ustake: Uint128::zero(),
            total_utoken: Uint128::zero(),
            exchange_rate: Decimal::one(),
            unlocked_coins: vec![],
            unbonding: Uint128::zero(),
            available: Uint128::zero(),
            tvl_utoken: Uint128::zero(),
        },
    );

    let res: PendingBatch = query_helper(deps.as_ref(), QueryMsg::PendingBatch {});
    assert_eq!(
        res,
        PendingBatch {
            id: 1,
            ustake_to_burn: Uint128::zero(),
            est_unbond_start_time: 269200, // 10,000 + 259,200
        },
    );
}

#[test]
fn bonding_check_received() -> StdResult<()> {
    let mut deps = setup_test();

    deps.querier.set_bank_balances(&[coin(100, COIN_UTOKEN), coin(200, COIN_IBC)]);
    deps.querier.set_cw20_balance(COIN_CW20, MOCK_CONTRACT_ADDR, 300);

    // Bond when no delegation has been made
    // In this case, the full deposit simply goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1000000, CONTRACT_DENOM)]),
        ExecuteMsg::Bond {
            receiver: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 3);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(Delegation::new("alice", 1000000).to_cosmos_msg(), 2)
    );
    assert_eq!(
        res.messages[1],
        SubMsg {
            id: 0,
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: "stake_token".to_string(),
                msg: to_binary(&Cw20ExecuteMsg::Mint {
                    recipient: "user_1".to_string(),
                    amount: Uint128::new(1000000)
                })
                .unwrap(),
                funds: vec![]
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never,
        }
    );

    let received = CallbackMsg::CheckReceivedCoins {
        snapshot: vec![coin(100, COIN_UTOKEN), coin(200, COIN_IBC), coin(300, COIN_CW20)],
    };

    assert_eq!(
        res.messages[2],
        SubMsg {
            id: 0,
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                msg: to_binary(&ExecuteMsg::Callback(received.clone())).unwrap(),
                funds: vec![]
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never
        }
    );

    deps.querier.set_bank_balances(&[coin(300, COIN_UTOKEN), coin(300, COIN_IBC)]);
    deps.querier.set_cw20_balance(COIN_CW20, MOCK_CONTRACT_ADDR, 350);

    // Bond when no delegation has been made
    // In this case, the full deposit simply goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(received),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);
    assert_eq!(
        res.events,
        vec![Event::new("erishub/callback_received_coins")
            .add_attribute("received_coin", "200ujuno")
            .add_attribute("received_coin", "100ibc/test")
            .add_attribute("received_coin", "50juno1cw20")]
    );

    let state = State::default();
    let reward_coins = state.unlocked_coins.load(deps.as_ref().storage)?;
    assert_eq!(reward_coins, &[coin(200, COIN_UTOKEN), coin(100, COIN_IBC), coin(50, COIN_CW20)]);

    //
    // HARVEST
    //
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667),
        Delegation::new("bob", 341667),
        Delegation::new("charlie", 341666),
    ]);
    let res = execute(deps.as_mut(), mock_env(), mock_info("worker", &[]), ExecuteMsg::Harvest {})
        .unwrap();

    assert_eq!(res.messages.len(), 5);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "alice".to_string(),
            }),
            2,
        )
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "bob".to_string(),
            }),
            2,
        )
    );
    assert_eq!(
        res.messages[2],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "charlie".to_string(),
            }),
            2,
        )
    );
    let received = CallbackMsg::CheckReceivedCoins {
        snapshot: vec![coin(300, COIN_UTOKEN), coin(300, COIN_IBC), coin(350, COIN_CW20)],
    };
    assert_eq!(
        res.messages[3],
        SubMsg {
            id: 0,
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                msg: to_binary(&ExecuteMsg::Callback(received.clone())).unwrap(),
                funds: vec![]
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never
        }
    );
    assert_eq!(
        res.messages[4],
        SubMsg {
            id: 0,
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                msg: to_binary(&ExecuteMsg::Callback(CallbackMsg::Reinvest {})).unwrap(),
                funds: vec![]
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never
        }
    );

    // received rewards through WithdrawDelegatorReward
    deps.querier.set_bank_balances(&[coin(1000, COIN_UTOKEN), coin(400, COIN_IBC)]);
    deps.querier.set_cw20_balance(COIN_CW20, MOCK_CONTRACT_ADDR, 450);

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(received),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);
    assert_eq!(
        res.events,
        vec![Event::new("erishub/callback_received_coins")
            .add_attribute("received_coin", "700ujuno")
            .add_attribute("received_coin", "100ibc/test")
            .add_attribute("received_coin", "100juno1cw20")]
    );

    let state = State::default();
    let reward_coins = state.unlocked_coins.load(deps.as_ref().storage)?;
    assert_eq!(reward_coins, &[coin(900, COIN_UTOKEN), coin(200, COIN_IBC), coin(150, COIN_CW20)]);

    //
    // REINVEST
    //

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 2);

    let total = Uint128::from(900u128);
    let fee =
        Decimal::from_ratio(1u128, 100u128).checked_mul_uint(total).expect("expects fee result");
    let delegated = total.saturating_sub(fee);

    assert_eq!(
        res.messages[0],
        SubMsg {
            id: 0,
            msg: Delegation::new("charlie", delegated.u128()).to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Never
        }
    );

    assert_eq!(
        res.messages[1],
        SubMsg {
            id: 0,
            msg: SendFee::new(Addr::unchecked("fee"), fee.u128()).to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Never
        }
    );

    // Storage should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![coin(200, COIN_IBC), coin(150, COIN_CW20)],);

    Ok(())
}
