use cosmwasm_std::{
    to_binary, Addr, BankMsg, Coin, CosmosMsg, Decimal, DepsMut, DistributionMsg, Env, Event,
    Order, Response, StdError, StdResult, SubMsg, SubMsgResponse, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use eris_staking::{CustomResponse, DecimalCheckedOps};

use eris_staking::hub::{
    Batch, CallbackMsg, ExecuteMsg, FeeConfig, InstantiateMsg, PendingBatch, UnbondRequest,
};

use crate::constants::{get_reward_fee_cap, CONTRACT_DENOM};
use crate::helpers::{dedupe, query_cw20_total_supply, query_delegation, query_delegations};
use crate::math::{
    compute_mint_amount, compute_redelegations_for_rebalancing, compute_redelegations_for_removal,
    compute_unbond_amount, compute_undelegations, mark_reconciled_batches, reconcile_batches,
};
use crate::state::State;
use crate::types::{Coins, Delegation, SendFee};

const CONTRACT_NAME: &str = "eris-hub";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

//--------------------------------------------------------------------------------------------------
// Instantiation
//--------------------------------------------------------------------------------------------------

pub fn instantiate(deps: DepsMut, env: Env, msg: InstantiateMsg) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let state = State::default();

    if msg.protocol_reward_fee.gt(&get_reward_fee_cap()) {
        return Err(StdError::generic_err("'protocol_reward_fee' greater than max"));
    }

    state.owner.save(deps.storage, &deps.api.addr_validate(&msg.owner)?)?;
    state.epoch_period.save(deps.storage, &msg.epoch_period)?;
    state.unbond_period.save(deps.storage, &msg.unbond_period)?;

    let mut validators = msg.validators;
    dedupe(&mut validators);

    state.validators.save(deps.storage, &validators)?;
    state.unlocked_coins.save(deps.storage, &vec![])?;
    state.fee_config.save(
        deps.storage,
        &FeeConfig {
            protocol_fee_contract: deps.api.addr_validate(&msg.protocol_fee_contract)?,
            protocol_reward_fee: msg.protocol_reward_fee,
        },
    )?;

    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: 1,
            ustake_to_burn: Uint128::zero(),
            est_unbond_start_time: env.block.time.seconds() + msg.epoch_period,
        },
    )?;

    Ok(Response::new().add_submessage(SubMsg::reply_on_success(
        CosmosMsg::Wasm(WasmMsg::Instantiate {
            admin: Some(msg.owner), // use the owner as admin for now; can be changed later by a `MsgUpdateAdmin`
            code_id: msg.cw20_code_id,
            msg: to_binary(&Cw20InstantiateMsg {
                name: msg.name,
                symbol: msg.symbol,
                decimals: msg.decimals,
                initial_balances: vec![],
                mint: Some(MinterResponse {
                    minter: env.contract.address.into(),
                    cap: None,
                }),
                marketing: None,
            })?,
            funds: vec![],
            label: "Eris Liquid Staking Token".to_string(),
        }),
        1,
    )))
}

pub fn register_stake_token(deps: DepsMut, response: SubMsgResponse) -> StdResult<Response> {
    let state = State::default();

    let event = response
        .events
        .iter()
        .find(|event| event.ty == "instantiate")
        .ok_or_else(|| StdError::generic_err("cannot find `instantiate` event"))?;

    let contract_addr_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "_contract_address")
        .ok_or_else(|| StdError::generic_err("cannot find `_contract_address` attribute"))?
        .value;

    let contract_addr = deps.api.addr_validate(contract_addr_str)?;
    state.stake_token.save(deps.storage, &contract_addr)?;

    Ok(Response::new())
}

//--------------------------------------------------------------------------------------------------
// Bonding and harvesting logics
//--------------------------------------------------------------------------------------------------

/// NOTE: In a previous implementation, we split up the deposited Token over all validators, so that
/// they all have the same amount of delegation. This is however quite gas-expensive: $1.5 cost in
/// the case of 15 validators.
///
/// To save gas for users, now we simply delegate all deposited Token to the validator with the
/// smallest amount of delegation. If delegations become severely unbalance as a result of this
/// (e.g. when a single user makes a very big deposit), anyone can invoke `ExecuteMsg::Rebalance`
/// to balance the delegations.
pub fn bond(
    deps: DepsMut,
    env: Env,
    receiver: Addr,
    token_to_bond: Uint128,
    donate: bool,
) -> StdResult<Response> {
    let state = State::default();
    let stake_token = state.stake_token.load(deps.storage)?;
    let validators = state.validators.load(deps.storage)?;

    // Query the current delegations made to validators, and find the validator with the smallest
    // delegated amount through a linear search
    // The code for linear search is a bit uglier than using `sort_by` but cheaper: O(n) vs O(n * log(n))
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let mut validator = &delegations[0].validator;
    let mut amount = delegations[0].amount;
    for d in &delegations[1..] {
        if d.amount < amount {
            validator = &d.validator;
            amount = d.amount;
        }
    }
    let new_delegation = Delegation {
        validator: validator.clone(),
        amount: token_to_bond.u128(),
    };

    // Query the current supply of Staking Token and compute the amount to mint
    let ustake_supply = query_cw20_total_supply(&deps.querier, &stake_token)?;
    let ustake_to_mint = if donate {
        Uint128::zero()
    } else {
        compute_mint_amount(ustake_supply, token_to_bond, &delegations)
    };

    let event = Event::new("erishub/bonded")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("receiver", receiver.clone())
        .add_attribute("token_bonded", token_to_bond)
        .add_attribute("ustake_minted", ustake_to_mint);

    let mint_msg = if donate {
        None
    } else {
        Some(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: stake_token.into(),
            msg: to_binary(&Cw20ExecuteMsg::Mint {
                recipient: receiver.to_string(),
                amount: ustake_to_mint,
            })?,
            funds: vec![],
        }))
    };

    Ok(Response::new()
        .add_message(new_delegation.to_cosmos_msg())
        .add_optional_message(mint_msg)
        .add_message(check_received_coin_msg(&deps, &env, Some(token_to_bond))?)
        .add_event(event)
        .add_attribute("action", "erishub/bond"))
}

pub fn harvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    let withdraw_msgs = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .filter(|d| !d.amount.amount.is_zero())
        .map(|d| {
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: d.validator,
            })
        })
        .collect::<Vec<_>>();

    Ok(Response::new()
        .add_messages(withdraw_msgs)
        .add_message(check_received_coin_msg(&deps, &env, None)?)
        .add_message(CallbackMsg::Reinvest {}.into_cosmos_msg(&env.contract.address)?)
        .add_attribute("action", "erishub/harvest"))
}

/// This callback is used to take a current snapshot of the balance and add the received balance to the unlocked_coins state after the execution
fn check_received_coin_msg(
    deps: &DepsMut,
    env: &Env,
    // offset to account for funds being sent that should be ignored
    negative_offset: Option<Uint128>,
) -> StdResult<CosmosMsg> {
    let mut amount =
        deps.querier.query_balance(env.contract.address.to_string(), CONTRACT_DENOM)?.amount;

    if let Some(negative_offset) = negative_offset {
        amount = amount.checked_sub(negative_offset)?;
    }

    CallbackMsg::CheckReceivedCoin {
        // 0. take current balance - offset
        snapshot: Coin {
            denom: CONTRACT_DENOM.to_string(),
            amount,
        },
    }
    .into_cosmos_msg(&env.contract.address)
}

/// NOTE:
/// 1. When delegation Token here, we don't need to use a `SubMsg` to handle the received coins,
/// because we have already withdrawn all claimable staking rewards previously in the same atomic
/// execution.
/// 2. Same as with `bond`, in the latest implementation we only delegate staking rewards with the
/// validator that has the smallest delegation amount.
pub fn reinvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let validators = state.validators.load(deps.storage)?;
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;
    let fee_config = state.fee_config.load(deps.storage)?;

    let utoken_available = unlocked_coins
        .iter()
        .find(|coin| coin.denom == CONTRACT_DENOM)
        .ok_or_else(|| {
            StdError::generic_err(format!("no {} available to be bonded", CONTRACT_DENOM))
        })?
        .amount;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let mut validator = &delegations[0].validator;
    let mut amount = delegations[0].amount;
    for d in &delegations[1..] {
        if d.amount < amount {
            validator = &d.validator;
            amount = d.amount;
        }
    }

    let protocol_fee_amount = fee_config.protocol_reward_fee.checked_mul_uint(utoken_available)?;
    let utoken_to_bond = utoken_available.saturating_sub(protocol_fee_amount);

    let new_delegation = Delegation::new(validator, utoken_to_bond.u128());

    unlocked_coins.retain(|coin| coin.denom != CONTRACT_DENOM);
    state.unlocked_coins.save(deps.storage, &unlocked_coins)?;

    let event = Event::new("erishub/harvested")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("utoken_bonded", utoken_to_bond)
        .add_attribute("utoken_protocol_fee", protocol_fee_amount);

    let mut msgs = vec![new_delegation.to_cosmos_msg()];

    if !protocol_fee_amount.is_zero() {
        let send_fee = SendFee::new(fee_config.protocol_fee_contract, protocol_fee_amount.u128());
        msgs.push(send_fee.to_cosmos_msg());
    }

    Ok(Response::new()
        .add_messages(msgs)
        .add_event(event)
        .add_attribute("action", "erishub/reinvest"))
}

pub fn callback_received_coin(deps: DepsMut, env: Env, snapshot: Coin) -> StdResult<Response> {
    // in some cosmwasm versions the events are not received in the callback
    // so each time the contract can receive some coins from rewards we also need to check after receiving some and add them to the unlocked_coins

    let mut received_coins = Coins(vec![]);
    let mut event = Event::new("erishub/callback_received_coins");
    let current_balance =
        deps.querier.query_balance(env.contract.address, snapshot.denom.to_string())?.amount;

    if current_balance > snapshot.amount {
        let amount = current_balance.checked_sub(snapshot.amount)?;
        event = event.add_attribute("received_coin", amount.to_string() + snapshot.denom.as_str());
        received_coins.add(&Coin::new(amount.u128(), snapshot.denom))?;
    }

    let state = State::default();
    state.unlocked_coins.update(deps.storage, |coins| -> StdResult<_> {
        let mut coins = Coins(coins);
        coins.add_many(&received_coins)?;
        Ok(coins.0)
    })?;

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/callback_received_coins"))
}

//--------------------------------------------------------------------------------------------------
// Unbonding logics
//--------------------------------------------------------------------------------------------------

pub fn queue_unbond(
    deps: DepsMut,
    env: Env,
    receiver: Addr,
    ustake_to_burn: Uint128,
) -> StdResult<Response> {
    let state = State::default();

    let mut pending_batch = state.pending_batch.load(deps.storage)?;
    pending_batch.ustake_to_burn += ustake_to_burn;
    state.pending_batch.save(deps.storage, &pending_batch)?;

    state.unbond_requests.update(
        deps.storage,
        (pending_batch.id, &receiver),
        |x| -> StdResult<_> {
            let mut request = x.unwrap_or_else(|| UnbondRequest {
                id: pending_batch.id,
                user: receiver.clone(),
                shares: Uint128::zero(),
            });
            request.shares += ustake_to_burn;
            Ok(request)
        },
    )?;

    let mut msgs: Vec<CosmosMsg> = vec![];
    let mut start_time = pending_batch.est_unbond_start_time.to_string();
    if env.block.time.seconds() >= pending_batch.est_unbond_start_time {
        start_time = "immediate".to_string();
        msgs.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: env.contract.address.clone().into(),
            msg: to_binary(&ExecuteMsg::SubmitBatch {})?,
            funds: vec![],
        }));
    }

    let event = Event::new("erishub/unbond_queued")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("est_unbond_start_time", start_time)
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("receiver", receiver)
        .add_attribute("ustake_to_burn", ustake_to_burn);

    Ok(Response::new()
        .add_messages(msgs)
        .add_event(event)
        .add_attribute("action", "erishub/queue_unbond"))
}

pub fn submit_batch(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let stake_token = state.stake_token.load(deps.storage)?;
    let validators = state.validators.load(deps.storage)?;
    let unbond_period = state.unbond_period.load(deps.storage)?;
    let pending_batch = state.pending_batch.load(deps.storage)?;

    let current_time = env.block.time.seconds();
    if current_time < pending_batch.est_unbond_start_time {
        return Err(StdError::generic_err(format!(
            "batch can only be submitted for unbonding after {}",
            pending_batch.est_unbond_start_time
        )));
    }

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let ustake_supply = query_cw20_total_supply(&deps.querier, &stake_token)?;

    let utoken_to_unbond =
        compute_unbond_amount(ustake_supply, pending_batch.ustake_to_burn, &delegations);
    let new_undelegations = compute_undelegations(utoken_to_unbond, &delegations);

    state.previous_batches.save(
        deps.storage,
        pending_batch.id,
        &Batch {
            id: pending_batch.id,
            reconciled: false,
            total_shares: pending_batch.ustake_to_burn,
            utoken_unclaimed: utoken_to_unbond,
            est_unbond_end_time: current_time + unbond_period,
        },
    )?;

    let epoch_period = state.epoch_period.load(deps.storage)?;
    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: pending_batch.id + 1,
            ustake_to_burn: Uint128::zero(),
            est_unbond_start_time: current_time + epoch_period,
        },
    )?;

    let undelegate_msgs = new_undelegations.iter().map(|d| d.to_cosmos_msg()).collect::<Vec<_>>();

    let burn_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: stake_token.into(),
        msg: to_binary(&Cw20ExecuteMsg::Burn {
            amount: pending_batch.ustake_to_burn,
        })?,
        funds: vec![],
    });

    let event = Event::new("erishub/unbond_submitted")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("utoken_unbonded", utoken_to_unbond)
        .add_attribute("ustake_burned", pending_batch.ustake_to_burn);

    Ok(Response::new()
        .add_messages(undelegate_msgs)
        .add_message(burn_msg)
        .add_message(check_received_coin_msg(&deps, &env, None)?)
        .add_event(event)
        .add_attribute("action", "erishub/unbond"))
}

pub fn reconcile(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let current_time = env.block.time.seconds();

    // Load batches that have not been reconciled
    let all_batches = state
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    let mut batches = all_batches
        .into_iter()
        .filter(|b| current_time > b.est_unbond_end_time)
        .collect::<Vec<_>>();

    let utoken_expected_received: Uint128 = batches.iter().map(|b| b.utoken_unclaimed).sum();

    if utoken_expected_received.is_zero() {
        return Ok(Response::new());
    }

    let unlocked_coins = state.unlocked_coins.load(deps.storage)?;
    let utoken_expected_unlocked = Coins(unlocked_coins).find(CONTRACT_DENOM).amount;

    let utoken_expected = utoken_expected_received + utoken_expected_unlocked;
    let utoken_actual = deps.querier.query_balance(&env.contract.address, CONTRACT_DENOM)?.amount;

    if utoken_actual >= utoken_expected {
        mark_reconciled_batches(&mut batches);
        for batch in &batches {
            state.previous_batches.save(deps.storage, batch.id, batch)?;
        }
        let ids = batches.iter().map(|b| b.id.to_string()).collect::<Vec<_>>().join(",");
        let event = Event::new("erishub/reconciled")
            .add_attribute("ids", ids)
            .add_attribute("utoken_deducted", "0");
        return Ok(Response::new().add_event(event).add_attribute("action", "erishub/reconcile"));
    }

    let utoken_to_deduct = utoken_expected - utoken_actual;

    reconcile_batches(&mut batches, utoken_to_deduct);

    for batch in &batches {
        state.previous_batches.save(deps.storage, batch.id, batch)?;
    }

    let ids = batches.iter().map(|b| b.id.to_string()).collect::<Vec<_>>().join(",");

    let event = Event::new("erishub/reconciled")
        .add_attribute("ids", ids)
        .add_attribute("utoken_deducted", utoken_to_deduct.to_string());

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/reconcile"))
}

pub fn withdraw_unbonded(
    deps: DepsMut,
    env: Env,
    user: Addr,
    receiver: Addr,
) -> StdResult<Response> {
    let state = State::default();
    let current_time = env.block.time.seconds();

    // NOTE: If the user has too many unclaimed requests, this may not fit in the WASM memory...
    // However, this is practically never going to happen. Who would create hundreds of unbonding
    // requests and never claim them?
    let requests = state
        .unbond_requests
        .idx
        .user
        .prefix(user.to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    // NOTE: Token in the following batches are withdrawn it the batch:
    // - is a _previous_ batch, not a _pending_ batch
    // - is reconciled
    // - has finished unbonding
    // If not sure whether the batches have been reconciled, the user should first invoke `ExecuteMsg::Reconcile`
    // before withdrawing.
    let mut total_utoken_to_refund = Uint128::zero();
    let mut ids: Vec<String> = vec![];
    for request in &requests {
        if let Ok(mut batch) = state.previous_batches.load(deps.storage, request.id) {
            if batch.reconciled && batch.est_unbond_end_time < current_time {
                let utoken_to_refund =
                    batch.utoken_unclaimed.multiply_ratio(request.shares, batch.total_shares);

                ids.push(request.id.to_string());

                total_utoken_to_refund += utoken_to_refund;
                batch.total_shares -= request.shares;
                batch.utoken_unclaimed -= utoken_to_refund;

                if batch.total_shares.is_zero() {
                    state.previous_batches.remove(deps.storage, request.id)?;
                } else {
                    state.previous_batches.save(deps.storage, batch.id, &batch)?;
                }

                state.unbond_requests.remove(deps.storage, (request.id, &user))?;
            }
        }
    }

    if total_utoken_to_refund.is_zero() {
        return Err(StdError::generic_err("withdrawable amount is zero"));
    }

    let refund_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: receiver.clone().into(),
        amount: vec![Coin::new(total_utoken_to_refund.u128(), CONTRACT_DENOM)],
    });

    let event = Event::new("erishub/unbonded_withdrawn")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("ids", ids.join(","))
        .add_attribute("user", user)
        .add_attribute("receiver", receiver)
        .add_attribute("utoken_refunded", total_utoken_to_refund);

    Ok(Response::new()
        .add_message(refund_msg)
        .add_event(event)
        .add_attribute("action", "erishub/withdraw_unbonded"))
}

//--------------------------------------------------------------------------------------------------
// Ownership and management logics
//--------------------------------------------------------------------------------------------------

pub fn rebalance(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let validators = state.validators.load(deps.storage)?;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;

    let new_redelegations = compute_redelegations_for_rebalancing(&delegations);

    let redelegate_msgs = new_redelegations.iter().map(|rd| rd.to_cosmos_msg()).collect::<Vec<_>>();

    let amount: u128 = new_redelegations.iter().map(|rd| rd.amount).sum();

    let event = Event::new("erishub/rebalanced").add_attribute("utoken_moved", amount.to_string());

    Ok(Response::new()
        .add_messages(redelegate_msgs)
        .add_message(check_received_coin_msg(&deps, &env, None)?)
        .add_event(event)
        .add_attribute("action", "erishub/rebalance"))
}

pub fn add_validator(deps: DepsMut, sender: Addr, validator: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    state.validators.update(deps.storage, |mut validators| {
        if validators.contains(&validator) {
            return Err(StdError::generic_err("validator is already whitelisted"));
        }
        validators.push(validator.clone());
        Ok(validators)
    })?;

    let event = Event::new("erishub/validator_added").add_attribute("validator", validator);

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/add_validator"))
}

pub fn remove_validator(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    let validators = state.validators.update(deps.storage, |mut validators| {
        if !validators.contains(&validator) {
            return Err(StdError::generic_err("validator is not already whitelisted"));
        }
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address)?;
    let delegation_to_remove = query_delegation(&deps.querier, &validator, &env.contract.address)?;
    let new_redelegations = compute_redelegations_for_removal(&delegation_to_remove, &delegations);

    let redelegate_msgs = new_redelegations.iter().map(|d| d.to_cosmos_msg()).collect::<Vec<_>>();

    let event = Event::new("erishub/validator_removed").add_attribute("validator", validator);

    Ok(Response::new()
        .add_messages(redelegate_msgs)
        .add_message(check_received_coin_msg(&deps, &env, None)?)
        .add_event(event)
        .add_attribute("action", "erishub/remove_validator"))
}

pub fn transfer_ownership(deps: DepsMut, sender: Addr, new_owner: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.new_owner.save(deps.storage, &deps.api.addr_validate(&new_owner)?)?;

    Ok(Response::new().add_attribute("action", "erishub/transfer_ownership"))
}

pub fn accept_ownership(deps: DepsMut, sender: Addr) -> StdResult<Response> {
    let state = State::default();

    let previous_owner = state.owner.load(deps.storage)?;
    let new_owner = state.new_owner.load(deps.storage)?;

    if sender != new_owner {
        return Err(StdError::generic_err("unauthorized: sender is not new owner"));
    }

    state.owner.save(deps.storage, &sender)?;
    state.new_owner.remove(deps.storage);

    let event = Event::new("erishub/ownership_transferred")
        .add_attribute("new_owner", new_owner)
        .add_attribute("previous_owner", previous_owner);

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/transfer_ownership"))
}

pub fn update_config(
    deps: DepsMut,
    sender: Addr,
    protocol_fee_contract: Option<String>,
    protocol_reward_fee: Option<Decimal>,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    if protocol_fee_contract.is_some() || protocol_reward_fee.is_some() {
        let mut fee_config = state.fee_config.load(deps.storage)?;

        if let Some(protocol_fee_contract) = protocol_fee_contract {
            fee_config.protocol_fee_contract = deps.api.addr_validate(&protocol_fee_contract)?;
        }

        if let Some(protocol_reward_fee) = protocol_reward_fee {
            if protocol_reward_fee.gt(&get_reward_fee_cap()) {
                return Err(StdError::generic_err("'protocol_reward_fee' greater than max"));
            }
            fee_config.protocol_reward_fee = protocol_reward_fee;
        }

        state.fee_config.save(deps.storage, &fee_config)?;
    }

    Ok(Response::new().add_attribute("action", "erishub/update_config"))
}
