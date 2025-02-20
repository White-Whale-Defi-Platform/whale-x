use std::collections::HashMap;
use std::{cmp, vec};

use astroport::asset::{Asset, AssetInfo};
use cosmwasm_std::{
    attr, to_json_binary, Addr, Attribute, BankMsg, Coin, CosmosMsg, Decimal, DepsMut, Env, Event,
    Order, Response, StdResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use eris::alliance_lst::{AllianceStakeToken, InstantiateMsg, Undelegation};
use eris::helper::validate_received_funds;
use eris::{CustomEvent, CustomMsgExt, CustomResponse, DecimalCheckedOps};

use eris::adapters::pair::Pair;
use eris::hub::{
    Batch, CallbackMsg, DelegationStrategy, ExecuteMsg, FeeConfig, PendingBatch, SingleSwapConfig,
    UnbondRequest,
};
use eris_chain_adapter::types::{
    chain, get_balances_hashmap, AssetExt, AssetInfoExt, CustomMsgType, CustomQueryType, DenomType,
    HubChainConfig, StageType, WithdrawType,
};

use itertools::Itertools;

use crate::constants::get_reward_fee_cap;
use crate::error::{ContractError, ContractResult};
use crate::helpers::{get_wanted_delegations, query_all_delegations, query_delegations};
use crate::math::{
    compute_mint_amount, compute_redelegations_for_rebalancing, compute_unbond_amount,
    compute_undelegations, get_utoken_per_validator, mark_reconciled_batches, reconcile_batches,
};
use crate::state::State;
use crate::types::alliance_delegations::AllianceDelegations;
use crate::types::gauges::TuneInfoGaugeLoader;
use crate::types::{withdraw_delegator_reward_msg, Coins, Delegation, SendFee, UndelegationExt};

use eris_chain_shared::chain_trait::ChainInterface;

const CONTRACT_NAME: &str = "eris-alliance-lst";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

//--------------------------------------------------------------------------------------------------
// Instantiation
//--------------------------------------------------------------------------------------------------

pub fn instantiate(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    msg: InstantiateMsg,
) -> ContractResult {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let state = State::default();
    let chain = chain(&env);

    if msg.protocol_reward_fee.gt(&get_reward_fee_cap()) {
        return Err(ContractError::ProtocolRewardFeeTooHigh {});
    }

    if msg.epoch_period == 0 {
        return Err(ContractError::CantBeZero("epoch_period".into()));
    }

    if msg.unbond_period == 0 {
        return Err(ContractError::CantBeZero("unbond_period".into()));
    }

    state.owner.save(deps.storage, &deps.api.addr_validate(&msg.owner)?)?;
    state.operator.save(deps.storage, &deps.api.addr_validate(&msg.operator)?)?;
    state.epoch_period.save(deps.storage, &msg.epoch_period)?;
    state.unbond_period.save(deps.storage, &msg.unbond_period)?;
    state.alliance_delegations.save(
        deps.storage,
        &AllianceDelegations {
            delegations: HashMap::new(),
        },
    )?;

    state.whale_btc_pool.save(deps.storage, &deps.api.addr_validate(&msg.whale_btc_pool)?)?;
    state.btc_denom.save(deps.storage, &msg.btc_denom)?;
    state.whale_denom.save(deps.storage, &msg.whale_denom)?;

    // by default donations are set to false
    state.allow_donations.save(deps.storage, &false)?;
    state.validator_proxy.save(deps.storage, &deps.api.addr_validate(&msg.validator_proxy)?)?;
    state.unlocked_coins.save(deps.storage, &vec![])?;
    state.fee_config.save(
        deps.storage,
        &FeeConfig {
            protocol_fee_contract: deps.api.addr_validate(&msg.protocol_fee_contract)?,
            protocol_reward_fee: msg.protocol_reward_fee,
        },
    )?;

    let validators = state.get_validators(deps.storage, &deps.querier)?;

    state.pending_batch.save(
        deps.storage,
        &PendingBatch {
            id: 1,
            ustake_to_burn: Uint128::zero(),
            est_unbond_start_time: env.block.time.seconds() + msg.epoch_period,
        },
    )?;

    let delegation_strategy = msg.delegation_strategy.unwrap_or(DelegationStrategy::Uniform);
    state
        .delegation_strategy
        .save(deps.storage, &delegation_strategy.validate(deps.api, &validators)?)?;

    let sub_denom = msg.denom;
    let full_denom = chain.get_token_denom(env.contract.address, sub_denom.clone());
    state.stake_token.save(
        deps.storage,
        &AllianceStakeToken {
            utoken: msg.utoken,
            denom: full_denom.clone(),
            total_supply: Uint128::zero(),
            total_utoken_bonded: Uint128::zero(),
        },
    )?;

    Ok(Response::new().add_message(chain.create_denom_msg(full_denom, sub_denom)))
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
    deps: DepsMut<CustomQueryType>,
    env: Env,
    receiver: Addr,
    funds: &[Coin],
    donate: bool,
) -> ContractResult {
    let state = State::default();
    let mut stake = state.stake_token.load(deps.storage)?;
    let alliance_delegations = state.alliance_delegations.load(deps.storage)?;

    let token_to_bond = validate_received_funds(funds, &stake.utoken)?;

    let new_delegation = find_new_delegation(
        &state,
        &deps,
        &env,
        &alliance_delegations,
        token_to_bond,
        &stake.utoken,
    )?;

    // Query the current supply of Staking Token and compute the amount to mint
    let ustake_supply = stake.total_supply;
    let ustake_to_mint = if donate {
        match state.allow_donations.may_load(deps.storage)? {
            Some(false) => Err(ContractError::DonationsDisabled {})?,
            Some(true) | None => {
                // if it is not set (backward compatibility) or set to true, donations are allowed
            },
        }
        Uint128::zero()
    } else {
        compute_mint_amount(ustake_supply, token_to_bond, stake.total_utoken_bonded)
    };

    let event = Event::new("erishub/bonded")
        .add_attribute("receiver", receiver.clone())
        .add_attribute("token_bonded", token_to_bond)
        .add_attribute("ustake_minted", ustake_to_mint);

    let mint_msgs: Option<Vec<CosmosMsg<CustomMsgType>>> = if donate {
        None
    } else {
        // create mint message and add to stored total supply
        stake.total_supply = stake.total_supply.checked_add(ustake_to_mint)?;

        Some(chain(&env).create_mint_msgs(stake.denom.clone(), ustake_to_mint, receiver))
    };

    stake.total_utoken_bonded = stake.total_utoken_bonded.checked_add(token_to_bond)?;
    state.stake_token.save(deps.storage, &stake)?;
    alliance_delegations.delegate(&new_delegation)?.save(&state, deps.storage)?;

    Ok(Response::new()
        .add_message(new_delegation.to_cosmos_msg(env.contract.address.to_string()))
        .add_optional_messages(mint_msgs)
        .add_message(check_received_coin_msg(&deps, &env, stake, Some(token_to_bond))?)
        .add_event(event)
        .add_attribute("action", "erishub/bond"))
}

pub fn harvest(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    validators: Option<Vec<String>>,
    withdrawals: Option<Vec<(WithdrawType, DenomType)>>,
    stages: Option<Vec<Vec<SingleSwapConfig>>>,
    _sender: Addr,
) -> ContractResult {
    if stages.is_some() || withdrawals.is_some() {
        return Err(ContractError::NotSupported("not support".to_string()));
    }

    let state = State::default();
    let stake = state.stake_token.load(deps.storage)?;

    // 1. Withdraw delegation rewards
    let withdraw_submsgs: Vec<CosmosMsg<CustomMsgType>> = if let Some(validators) = validators {
        // it is validated by the cosmos sdk that validators exist
        validators
            .into_iter()
            .map(|validator| {
                withdraw_delegator_reward_msg(
                    env.contract.address.to_string(),
                    validator,
                    stake.utoken.to_string(),
                )
            })
            .collect()
    } else {
        query_all_delegations(
            &state.alliance_delegations.load(deps.storage)?,
            &deps.querier,
            &env.contract.address,
            &stake.utoken,
        )?
        .into_iter()
        .map(|d| {
            withdraw_delegator_reward_msg(
                env.contract.address.to_string(),
                d.validator,
                stake.utoken.to_string(),
            )
        })
        .collect::<Vec<_>>()
    };
    Ok(Response::new()
        .add_messages(withdraw_submsgs)
        .add_callback(&env, CallbackMsg::HalfSwapReward {})?
        .add_callback(&env, CallbackMsg::ProvideLiquidity {})?
        .add_message(check_received_coin_msg(
            &deps,
            &env,
            state.stake_token.load(deps.storage)?,
            None,
        )?)
        .add_callback(
            &env,
            CallbackMsg::Reinvest {
                skip_fee: false,
            },
        )?
        .add_attribute("action", "erishub/harvest"))
}

/// this method will split LP positions into each single position
pub fn withdraw_lps(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    withdrawals: Vec<(WithdrawType, DenomType)>,
) -> ContractResult {
    let mut withdraw_msgs: Vec<CosmosMsg<CustomMsgType>> = vec![];
    let chain = chain(&env);
    let get_denoms = || withdrawals.iter().map(|a| a.1.clone()).collect_vec();
    let balances = get_balances_hashmap(&deps, env, get_denoms)?;
    let get_chain_config = || Ok(HubChainConfig {});

    for (withdraw_type, denom) in withdrawals {
        let balance = balances.get(&denom.to_string());

        if let Some(balance) = balance {
            if !balance.is_zero() {
                let msg =
                    chain.create_withdraw_msg(get_chain_config, withdraw_type, denom, *balance)?;
                if let Some(msg) = msg {
                    withdraw_msgs.push(msg);
                }
            }
        }
    }

    Ok(Response::new().add_messages(withdraw_msgs).add_attribute("action", "erishub/withdraw_lps"))
}

/// swaps all unlocked coins to token
pub fn single_stage_swap(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    stage: Vec<SingleSwapConfig>,
    index: usize,
) -> ContractResult {
    let state = State::default();
    let chain = chain(&env);
    let default_max_spread = state.get_default_max_spread(deps.storage);
    let get_chain_config = || Ok(HubChainConfig {});
    let get_denoms = || stage.iter().map(|a| a.1.clone()).collect_vec();
    let balances: HashMap<String, Uint128> = get_balances_hashmap(&deps, env, get_denoms)?;

    let mut response = Response::new().add_attribute("action", "erishub/single_stage_swap");
    // iterate all specified swaps of the stage
    for (stage_type, denom, belief_price, max_amount, fee) in stage {
        let balance = balances.get(&denom.to_string());
        // check if the swap also has a balance in the contract
        if let Some(mut available) = balance.cloned() {
            if !available.is_zero() {
                if fee == Some(true) {
                    if index != 0 {
                        return Err(ContractError::FeePaymentNotAllowed {});
                    }

                    let fee_config = state.fee_config.load(deps.storage)?;
                    let protocol_fee =
                        fee_config.protocol_reward_fee.checked_mul_uint(available)?;
                    available = available.saturating_sub(protocol_fee);

                    let send_fee = denom
                        .with_balance(protocol_fee)
                        .into_msg(&fee_config.protocol_fee_contract)?
                        .to_specific()?;
                    response = response.add_message(send_fee)
                }

                let used_amount = match max_amount {
                    Some(max_amount) => {
                        if max_amount.is_zero() {
                            available
                        } else {
                            cmp::min(available, max_amount)
                        }
                    },
                    None => available,
                };

                // create a single swap message add add to submsgs
                let msg = chain.create_single_stage_swap_msgs(
                    get_chain_config,
                    stage_type,
                    denom,
                    used_amount,
                    belief_price,
                    default_max_spread,
                )?;
                response = response.add_message(msg)
            }
        }
    }

    Ok(response)
}

#[allow(clippy::cmp_owned)]
fn validate_no_utoken_or_ustake_swap(
    stages: &Option<Vec<Vec<SingleSwapConfig>>>,
    stake_token: &AllianceStakeToken,
) -> Result<(), ContractError> {
    if let Some(stages) = stages {
        for stage in stages {
            for (_addr, denom, _, _, _) in stage {
                if denom.to_string() == stake_token.utoken || denom.to_string() == stake_token.denom
                {
                    return Err(ContractError::SwapFromNotAllowed(denom.to_string()));
                }
            }
        }
    }
    Ok(())
}

fn validate_no_belief_price(stages: &Vec<Vec<SingleSwapConfig>>) -> Result<(), ContractError> {
    for stage in stages {
        for (_, _, belief_price, _, _) in stage {
            if belief_price.is_some() {
                return Err(ContractError::BeliefPriceNotAllowed {});
            }
        }
    }
    Ok(())
}

pub fn half_swap_reward_msg(deps: &DepsMut<CustomQueryType>, env: &Env) -> ContractResult {
    let state = State::default();
    let whale_denom = state.whale_denom.load(deps.storage)?;
    let amount = deps.querier.query_balance(env.contract.address.to_string(), &whale_denom)?.amount;
    let pool = state.whale_btc_pool.load(deps.storage)?;
    let amount = amount.checked_div(Uint128::new(2)).unwrap();
    let whale_denom = state.whale_denom.load(deps.storage)?;

    if amount == Uint128::zero() {
        return Err(ContractError::NoReward {});
    }

    let swap_config = (
        StageType::Dex {
            addr: pool,
        },
        DenomType::native(whale_denom),
        None, // price
        Some(amount),
        None,
    );

    let response = Response::new().add_message(
        CallbackMsg::SingleStageSwap {
            stage: vec![swap_config],
            index: 0,
        }
        .into_cosmos_msg(&env.contract.address)?,
    );

    Ok(response)
}

pub fn provide_liquidity_msg(deps: &DepsMut<CustomQueryType>, env: &Env) -> ContractResult {
    let state = State::default();
    let whale_denom = state.whale_denom.load(deps.storage)?;
    let btc_denom = state.btc_denom.load(deps.storage)?;
    let whale_btc_pool = state.whale_btc_pool.load(deps.storage)?;

    let whale_amount =
        deps.querier.query_balance(env.contract.address.to_string(), &whale_denom)?.amount;
    let btc_amount =
        deps.querier.query_balance(env.contract.address.to_string(), &btc_denom)?.amount;

    let mut assets: Vec<Asset> = vec![];
    let mut funds: Vec<Coin> = vec![];

    if whale_amount.is_zero() || btc_amount.is_zero() {
        return Err(ContractError::NoReward {});
    }

    assets.push(Asset {
        info: AssetInfo::NativeToken {
            denom: whale_denom.clone(),
        },
        amount: whale_amount,
    });
    assets.push(Asset {
        info: AssetInfo::NativeToken {
            denom: btc_denom.clone(),
        },
        amount: btc_amount,
    });
    funds.push(Coin {
        denom: whale_denom,
        amount: whale_amount,
    });
    funds.push(Coin {
        denom: btc_denom,
        amount: btc_amount,
    });

    let mut response = Response::new().add_attribute("action", "erishub/add_liquidity");
    response = response.add_message(
        Pair(whale_btc_pool)
            .provide_liquidity_msg(assets, None, Some(env.contract.address.to_string()), funds)?
            .to_specific()?,
    );
    Ok(response)
}

/// This callback is used to take a current snapshot of the balance and add the received balance to the unlocked_coins state after the execution
fn check_received_coin_msg(
    deps: &DepsMut<CustomQueryType>,
    env: &Env,
    stake: AllianceStakeToken,
    // offset to account for funds being sent that should be ignored
    negative_offset: Option<Uint128>,
) -> StdResult<CosmosMsg<CustomMsgType>> {
    let mut amount =
        deps.querier.query_balance(env.contract.address.to_string(), &stake.utoken)?.amount;

    if let Some(negative_offset) = negative_offset {
        amount = amount.checked_sub(negative_offset)?;
    }

    let amount_stake =
        deps.querier.query_balance(env.contract.address.to_string(), stake.denom.clone())?.amount;

    CallbackMsg::CheckReceivedCoin {
        // 0. take current balance - offset
        snapshot: Coin {
            denom: stake.utoken,
            amount,
        },
        snapshot_stake: Coin {
            denom: stake.denom,
            amount: amount_stake,
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
pub fn reinvest(deps: DepsMut<CustomQueryType>, env: Env, skip_fee: bool) -> ContractResult {
    let state = State::default();
    let fee_config = state.fee_config.load(deps.storage)?;
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;
    let mut stake = state.stake_token.load(deps.storage)?;
    let mut alliance_delegations = state.alliance_delegations.load(deps.storage)?;

    if unlocked_coins.is_empty() {
        return Err(ContractError::NoTokensAvailable(format!(
            "{0}, {1}",
            stake.utoken, stake.denom
        )));
    }

    let mut event = Event::new("erishub/harvested");
    let mut msgs: Vec<CosmosMsg<CustomMsgType>> = vec![];

    let protocol_reward_fee = if skip_fee {
        Decimal::zero()
    } else {
        fee_config.protocol_reward_fee
    };

    for coin in unlocked_coins.iter() {
        let available = coin.amount;
        let protocol_fee = protocol_reward_fee.checked_mul_uint(available)?;
        let remaining = available.saturating_sub(protocol_fee);

        let send_fee = if coin.denom == stake.utoken {
            let to_bond = remaining;
            // if receiving normal utoken -> restake
            let new_delegation = find_new_delegation(
                &state,
                &deps,
                &env,
                &alliance_delegations,
                to_bond,
                &stake.utoken,
            )?;

            event = event
                .add_attribute("utoken_bonded", to_bond)
                .add_attribute("utoken_protocol_fee", protocol_fee);

            stake.total_utoken_bonded += to_bond;
            alliance_delegations =
                alliance_delegations.delegate(&new_delegation)?.save(&state, deps.storage)?;
            msgs.push(new_delegation.to_cosmos_msg(env.contract.address.to_string()));
            true
        } else if coin.denom == stake.denom {
            // if receiving ustake (staked utoken) -> burn
            event = event
                .add_attribute("ustake_burned", remaining)
                .add_attribute("ustake_protocol_fee", protocol_fee);

            stake.total_supply = stake.total_supply.checked_sub(remaining)?;
            msgs.push(chain(&env).create_burn_msg(stake.denom.clone(), remaining));
            true
        } else {
            // we can ignore other coins as we will only store utoken and ustake there
            false
        };

        if send_fee && !protocol_fee.is_zero() {
            let send_fee = SendFee::new(
                fee_config.protocol_fee_contract.clone(),
                protocol_fee.u128(),
                coin.denom.clone(),
            );
            msgs.push(send_fee.to_cosmos_msg());
        }
    }

    state.stake_token.save(deps.storage, &stake)?;

    // remove the converted coins. Unlocked_coins track utoken ([TOKEN]) and ustake (amp[TOKEN]).
    unlocked_coins.retain(|coin| coin.denom != stake.utoken && coin.denom != stake.denom);
    state.unlocked_coins.save(deps.storage, &unlocked_coins)?;

    // update exchange_rate history
    let exchange_rate = calc_current_exchange_rate(stake)?;
    state.exchange_history.save(deps.storage, env.block.time.seconds(), &exchange_rate)?;

    Ok(Response::new()
        .add_messages(msgs)
        .add_event(event)
        .add_attribute("action", "erishub/reinvest")
        .add_attribute("exchange_rate", exchange_rate.to_string()))
}

fn calc_current_exchange_rate(stake: AllianceStakeToken) -> Result<Decimal, ContractError> {
    let exchange_rate = if stake.total_supply.is_zero() {
        Decimal::one()
    } else {
        Decimal::from_ratio(stake.total_utoken_bonded, stake.total_supply)
    };
    Ok(exchange_rate)
}

pub fn callback_received_coins(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    snapshot: Coin,
    snapshot_stake: Coin,
) -> ContractResult {
    let state = State::default();
    // in some cosmwasm versions the events are not received in the callback
    // so each time the contract can receive some coins from rewards we also need to check after receiving some and add them to the unlocked_coins

    let mut received_coins = Coins(vec![]);
    let mut event = Event::new("erishub/received");

    event = event.add_optional_attribute(add_to_received_coins(
        &deps,
        env.contract.address.clone(),
        snapshot,
        &mut received_coins,
    )?);

    event = event.add_optional_attribute(add_to_received_coins(
        &deps,
        env.contract.address,
        snapshot_stake,
        &mut received_coins,
    )?);

    if !received_coins.0.is_empty() {
        state.unlocked_coins.update(deps.storage, |coins| -> StdResult<_> {
            let mut coins = Coins(coins);
            coins.add_many(&received_coins)?;
            Ok(coins.0)
        })?;
    }

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/received"))
}

fn add_to_received_coins(
    deps: &DepsMut<CustomQueryType>,
    contract: Addr,
    snapshot: Coin,
    received_coins: &mut Coins,
) -> Result<Option<Attribute>, ContractError> {
    let current_balance = deps.querier.query_balance(contract, snapshot.denom.to_string())?.amount;

    let attr = if current_balance > snapshot.amount {
        let received_amount = current_balance.checked_sub(snapshot.amount)?;
        let received = Coin::new(received_amount.u128(), snapshot.denom);
        received_coins.add(&received)?;
        Some(attr("received_coin", received.to_string()))
    } else {
        None
    };

    Ok(attr)
}

/// searches for the validator with the least amount of delegations
/// For Uniform mode, searches through the validators list
/// For Gauge mode, searches for all delegations, and if nothing found, use the first validator from the list.
fn find_new_delegation(
    state: &State,
    deps: &DepsMut<CustomQueryType>,
    env: &Env,
    alliance_delegations: &AllianceDelegations,
    utoken_to_bond: Uint128,
    utoken: &String,
) -> Result<Delegation, ContractError> {
    let delegation_strategy =
        state.delegation_strategy.may_load(deps.storage)?.unwrap_or(DelegationStrategy::Uniform {});

    match delegation_strategy {
        DelegationStrategy::Uniform {} => {
            let validators = state.get_validators(deps.storage, &deps.querier)?;
            let delegations = query_delegations(
                alliance_delegations,
                &deps.querier,
                utoken,
                &validators,
                &env.contract.address,
            )?;

            // Query the current delegations made to validators, and find the validator with the smallest
            // delegated amount through a linear search
            // The code for linear search is a bit uglier than using `sort_by` but cheaper: O(n) vs O(n * log(n))
            let mut validator = &delegations[0].validator;
            let mut amount = delegations[0].amount;

            for d in &delegations[1..] {
                // when using uniform distribution, it is allowed to bond anywhere
                // otherwise bond only in one of the
                if d.amount < amount {
                    validator = &d.validator;
                    amount = d.amount;
                }
            }
            let new_delegation = Delegation::new(validator, utoken_to_bond.u128(), utoken);

            Ok(new_delegation)
        },
        DelegationStrategy::Gauges {
            ..
        }
        | DelegationStrategy::Defined {
            ..
        } => {
            let current_delegations = query_all_delegations(
                alliance_delegations,
                &deps.querier,
                &env.contract.address,
                utoken,
            )?;
            let utoken_staked: u128 = current_delegations.iter().map(|d| d.amount).sum();
            let validators = state.get_validators(deps.storage, &deps.querier)?;

            let (map, _, _, _) = get_utoken_per_validator(
                state,
                deps.storage,
                Uint128::new(utoken_staked).checked_add(utoken_to_bond)?.u128(),
                &validators,
                None,
            )?;

            let mut validator: Option<String> = None;
            let mut amount = Uint128::zero();

            for delegation in &current_delegations {
                let diff = map
                    .get(&delegation.validator)
                    .copied()
                    .unwrap_or_default()
                    .saturating_sub(Uint128::new(delegation.amount));

                if diff > amount || validator.is_none() {
                    validator = Some(delegation.validator.clone());
                    amount = diff;
                }
            }

            if validator.is_none() {
                validator = Some(validators.first().unwrap().to_string());
            }

            let new_delegation =
                Delegation::new(validator.unwrap().as_str(), utoken_to_bond.u128(), utoken);

            Ok(new_delegation)
        },
    }
}

//--------------------------------------------------------------------------------------------------
// Unbonding logics
//--------------------------------------------------------------------------------------------------

pub fn queue_unbond(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    receiver: Addr,
    ustake_to_burn: Uint128,
) -> ContractResult {
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

    let mut msgs: Vec<CosmosMsg<CustomMsgType>> = vec![];
    let mut start_time = pending_batch.est_unbond_start_time.to_string();
    if env.block.time.seconds() > pending_batch.est_unbond_start_time {
        start_time = "immediate".to_string();
        msgs.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: env.contract.address.into(),
            msg: to_json_binary(&ExecuteMsg::SubmitBatch {})?,
            funds: vec![],
        }));
    }

    let event = Event::new("erishub/unbond_queued")
        .add_attribute("est_unbond_start_time", start_time)
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("receiver", receiver)
        .add_attribute("ustake_to_burn", ustake_to_burn);

    Ok(Response::new()
        .add_messages(msgs)
        .add_event(event)
        .add_attribute("action", "erishub/queue_unbond"))
}

pub fn submit_batch(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    sender: Addr,
    undelegations: Option<Vec<Undelegation>>,
) -> ContractResult {
    let state = State::default();
    let mut stake = state.stake_token.load(deps.storage)?;
    let unbond_period = state.unbond_period.load(deps.storage)?;
    let pending_batch = state.pending_batch.load(deps.storage)?;
    let alliance_delegations = state.alliance_delegations.load(deps.storage)?;

    let current_time = env.block.time.seconds();
    if current_time < pending_batch.est_unbond_start_time {
        return Err(ContractError::SubmitBatchAfter(pending_batch.est_unbond_start_time));
    }

    let ustake_supply = stake.total_supply;

    let utoken_to_unbond = compute_unbond_amount(
        ustake_supply,
        pending_batch.ustake_to_burn,
        stake.total_utoken_bonded,
    );

    let new_undelegations = if let Some(undelegations) = undelegations {
        state.assert_operator(deps.storage, &sender)?;

        let provided_amount: Uint128 = undelegations.iter().map(|a| a.amount).sum();
        if provided_amount != utoken_to_unbond {
            return Err(ContractError::SubmitBatchFailure(format!(
                "provided amount {0} does not equal expected amount {1}",
                provided_amount, utoken_to_unbond
            )));
        }

        undelegations
    } else {
        let validators = state.get_validators(deps.storage, &deps.querier)?;
        let delegations = query_all_delegations(
            &alliance_delegations,
            &deps.querier,
            &env.contract.address,
            &stake.utoken,
        )?;

        compute_undelegations(
            &state,
            deps.storage,
            utoken_to_unbond,
            &delegations,
            validators,
            &stake.utoken,
        )?
    };

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

    // validates that the amount is available and validator delegation exists
    alliance_delegations.undelegate(&new_undelegations)?.save(&state, deps.storage)?;
    let undelegate_msgs = new_undelegations
        .into_iter()
        .map(|d| d.to_cosmos_msg(env.contract.address.to_string(), stake.utoken.clone()))
        .collect::<Vec<_>>();

    // apply burn to the stored total supply and save state
    stake.total_utoken_bonded = stake.total_utoken_bonded.checked_sub(utoken_to_unbond)?;
    stake.total_supply = stake.total_supply.checked_sub(pending_batch.ustake_to_burn)?;
    state.stake_token.save(deps.storage, &stake)?;

    let burn_msg: CosmosMsg<CustomMsgType> =
        chain(&env).create_burn_msg(stake.denom.clone(), pending_batch.ustake_to_burn);

    let event = Event::new("erishub/unbond_submitted")
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("utoken_unbonded", utoken_to_unbond)
        .add_attribute("ustake_burned", pending_batch.ustake_to_burn);

    Ok(Response::new()
        .add_messages(undelegate_msgs)
        .add_message(burn_msg)
        .add_message(check_received_coin_msg(&deps, &env, stake, None)?)
        .add_event(event)
        .add_attribute("action", "erishub/unbond"))
}

pub fn reconcile(deps: DepsMut<CustomQueryType>, env: Env) -> ContractResult {
    let state = State::default();
    let stake = state.stake_token.load(deps.storage)?;
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
    let utoken_expected_unlocked = Coins(unlocked_coins).find(&stake.utoken).amount;

    let utoken_expected = utoken_expected_received + utoken_expected_unlocked;
    let utoken_actual = deps.querier.query_balance(&env.contract.address, stake.utoken)?.amount;

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

    let reconcile_info = reconcile_batches(&mut batches, utoken_to_deduct);

    for batch in &batches {
        state.previous_batches.save(deps.storage, batch.id, batch)?;
    }

    let ids = batches.iter().map(|b| b.id.to_string()).collect::<Vec<_>>().join(",");

    let event = Event::new("erishub/reconciled")
        .add_attribute("ids", ids)
        .add_attribute("utoken_deducted", utoken_to_deduct.to_string())
        .add_optional_attribute(reconcile_info);

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/reconcile"))
}

pub fn check_slashing(
    deps: DepsMut<CustomQueryType>,
    _env: Env,
    sender: Addr,
    current_delegations: Vec<(String, Uint128)>,
    state_total_utoken_bonded: Uint128,
) -> ContractResult {
    let state = State::default();
    state.assert_owner_or_operator(deps.storage, &sender)?;

    let mut stake_token = state.stake_token.load(deps.storage)?;
    let alliance_delegations = state.alliance_delegations.load(deps.storage)?;
    let new_sum = Uint128::new(current_delegations.iter().map(|(_, amount)| amount.u128()).sum());

    if stake_token.total_utoken_bonded != state_total_utoken_bonded {
        return Err(ContractError::StateChanged("total_utoken_bonded".to_string()));
    }

    if alliance_delegations.delegations.len() != current_delegations.len() {
        return Err(ContractError::StateChanged("delegations".to_string()));
    }

    if new_sum < state_total_utoken_bonded.multiply_ratio(95u128, 100u128) {
        return Err(ContractError::StateChanged("big slash".to_string()));
    }

    let old = stake_token.total_utoken_bonded;
    stake_token.total_utoken_bonded = new_sum;

    let delegations = current_delegations.into_iter().collect::<HashMap<_, _>>();
    state.alliance_delegations.save(
        deps.storage,
        &AllianceDelegations {
            delegations,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "erishub/check_slashing")
        .add_attribute("old_utoken_bonded", old.to_string())
        .add_attribute("new_utoken_bonded", new_sum.to_string()))
}

pub fn withdraw_unbonded(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    user: Addr,
    receiver: Addr,
) -> ContractResult {
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
        return Err(ContractError::CantBeZero("withdrawable amount".into()));
    }
    let stake = state.stake_token.load(deps.storage)?;

    let refund_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: receiver.clone().into(),
        amount: vec![Coin::new(total_utoken_to_refund.u128(), stake.utoken)],
    });

    let event = Event::new("erishub/unbonded_withdrawn")
        .add_attribute("ids", ids.join(","))
        .add_attribute("user", user)
        .add_attribute("receiver", receiver)
        .add_attribute("utoken_refunded", total_utoken_to_refund);

    Ok(Response::new()
        .add_message(refund_msg)
        .add_event(event)
        .add_attribute("action", "erishub/withdraw_unbonded"))
}

pub fn tune_delegations(deps: DepsMut<CustomQueryType>, env: Env, sender: Addr) -> ContractResult {
    let state = State::default();
    state.assert_owner(deps.storage, &sender)?;
    let (wanted_delegations, save) =
        get_wanted_delegations(&state, &env, deps.storage, &deps.querier, TuneInfoGaugeLoader {})?;
    let attributes = if save {
        state.delegation_goal.save(deps.storage, &wanted_delegations)?;
        wanted_delegations
            .shares
            .iter()
            .map(|a| attr("goal_delegation", format!("{0}={1}", a.0, a.1)))
            .collect()
    } else {
        state.delegation_goal.remove(deps.storage);
        // these would be boring, as all are the same
        vec![]
    };
    Ok(Response::new()
        .add_attribute("action", "erishub/tune_delegations")
        .add_attributes(attributes))
}

//--------------------------------------------------------------------------------------------------
// Ownership and management logics
//--------------------------------------------------------------------------------------------------

pub fn rebalance(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    sender: Addr,
    min_redelegation: Option<Uint128>,
) -> ContractResult {
    let state = State::default();
    let stake = state.stake_token.load(deps.storage)?;
    let alliance_delegations = state.alliance_delegations.load(deps.storage)?;

    state.assert_owner(deps.storage, &sender)?;
    let validators = state.get_validators(deps.storage, &deps.querier)?;
    let delegations = query_all_delegations(
        &alliance_delegations,
        &deps.querier,
        &env.contract.address,
        &stake.utoken,
    )?;

    let min_redelegation = min_redelegation.unwrap_or_default();

    let new_redelegations = compute_redelegations_for_rebalancing(
        &state,
        deps.storage,
        &delegations,
        validators,
        &stake.utoken,
    )?
    .into_iter()
    .filter(|redelegation| redelegation.amount >= min_redelegation.u128())
    .collect::<Vec<_>>();

    alliance_delegations.redelegate(&new_redelegations)?.save(&state, deps.storage)?;
    let redelegate_msgs = new_redelegations
        .iter()
        .map(|rd| rd.to_cosmos_msg(env.contract.address.to_string()))
        .collect::<Vec<_>>();

    let amount: u128 = new_redelegations.iter().map(|rd| rd.amount).sum();

    let event = Event::new("erishub/rebalanced").add_attribute("utoken_moved", amount.to_string());

    let check_msg = if !redelegate_msgs.is_empty() {
        // only check coins if a redelegation is happening
        Some(check_received_coin_msg(&deps, &env, stake, None)?)
    } else {
        None
    };

    Ok(Response::new()
        .add_messages(redelegate_msgs)
        .add_optional_message(check_msg)
        .add_event(event)
        .add_attribute("action", "erishub/rebalance"))
}

pub fn transfer_ownership(
    deps: DepsMut<CustomQueryType>,
    sender: Addr,
    new_owner: String,
) -> ContractResult {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.new_owner.save(deps.storage, &deps.api.addr_validate(&new_owner)?)?;

    Ok(Response::new().add_attribute("action", "erishub/transfer_ownership"))
}

pub fn drop_ownership_proposal(deps: DepsMut<CustomQueryType>, sender: Addr) -> ContractResult {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.new_owner.remove(deps.storage);

    Ok(Response::new().add_attribute("action", "erishub/drop_ownership_proposal"))
}

pub fn accept_ownership(deps: DepsMut<CustomQueryType>, sender: Addr) -> ContractResult {
    let state = State::default();

    let previous_owner = state.owner.load(deps.storage)?;
    let new_owner = state.new_owner.load(deps.storage)?;

    if sender != new_owner {
        return Err(ContractError::UnauthorizedSenderNotNewOwner {});
    }

    state.owner.save(deps.storage, &sender)?;
    state.new_owner.remove(deps.storage);

    let event = Event::new("erishub/ownership_transferred")
        .add_attribute("new_owner", new_owner)
        .add_attribute("previous_owner", previous_owner);

    Ok(Response::new().add_event(event).add_attribute("action", "erishub/transfer_ownership"))
}

#[allow(clippy::too_many_arguments)]
pub fn update_config(
    deps: DepsMut<CustomQueryType>,
    sender: Addr,
    protocol_fee_contract: Option<String>,
    protocol_reward_fee: Option<Decimal>,
    operator: Option<String>,
    stages_preset: Option<Vec<Vec<SingleSwapConfig>>>,
    withdrawals_preset: Option<Vec<(WithdrawType, DenomType)>>,
    allow_donations: Option<bool>,
    delegation_strategy: Option<DelegationStrategy>,
    default_max_spread: Option<u64>,
    epoch_period: Option<u64>,
    unbond_period: Option<u64>,
    validator_proxy: Option<String>,
    whale_denom: Option<String>,
    btc_denom: Option<String>,
    whale_btc_pool: Option<Addr>,
) -> ContractResult {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    if protocol_fee_contract.is_some() || protocol_reward_fee.is_some() {
        let mut fee_config = state.fee_config.load(deps.storage)?;

        if let Some(protocol_fee_contract) = protocol_fee_contract {
            fee_config.protocol_fee_contract = deps.api.addr_validate(&protocol_fee_contract)?;
        }

        if let Some(protocol_reward_fee) = protocol_reward_fee {
            if protocol_reward_fee.gt(&get_reward_fee_cap()) {
                return Err(ContractError::ProtocolRewardFeeTooHigh {});
            }
            fee_config.protocol_reward_fee = protocol_reward_fee;
        }

        state.fee_config.save(deps.storage, &fee_config)?;
    }

    if let Some(epoch_period) = epoch_period {
        if epoch_period == 0 {
            return Err(ContractError::CantBeZero("epoch_period".into()));
        }
        state.epoch_period.save(deps.storage, &epoch_period)?;
    }

    if let Some(unbond_period) = unbond_period {
        if unbond_period == 0 {
            return Err(ContractError::CantBeZero("unbond_period".into()));
        }
        state.unbond_period.save(deps.storage, &unbond_period)?;
    }

    if let Some(operator) = operator {
        state.operator.save(deps.storage, &deps.api.addr_validate(operator.as_str())?)?;
    }

    if let Some(validator_proxy) = validator_proxy {
        state
            .validator_proxy
            .save(deps.storage, &deps.api.addr_validate(validator_proxy.as_str())?)?;
    }

    if stages_preset.is_some() {
        validate_no_utoken_or_ustake_swap(&stages_preset, &state.stake_token.load(deps.storage)?)?;
    }

    if let Some(stages_preset) = stages_preset {
        // belief price is not allowed. We still store it with None, as otherwise a lot of additional logic is required to load it.
        validate_no_belief_price(&stages_preset)?;
        state.stages_preset.save(deps.storage, &stages_preset)?;
    }

    if let Some(withdrawals_preset) = withdrawals_preset {
        state.withdrawals_preset.save(deps.storage, &withdrawals_preset)?;
    }

    if let Some(delegation_strategy) = delegation_strategy {
        let validators = state.get_validators(deps.storage, &deps.querier)?;
        state
            .delegation_strategy
            .save(deps.storage, &delegation_strategy.validate(deps.api, &validators)?)?;
    }

    if let Some(allow_donations) = allow_donations {
        state.allow_donations.save(deps.storage, &allow_donations)?;
    }
    if let Some(default_max_spread) = default_max_spread {
        state.default_max_spread.save(deps.storage, &default_max_spread)?;
    }

    if let Some(whale_denom) = whale_denom {
        state.whale_denom.save(deps.storage, &whale_denom)?;
    }
    if let Some(btc_denom) = btc_denom {
        state.btc_denom.save(deps.storage, &btc_denom)?;
    }
    if let Some(whale_btc_pool) = whale_btc_pool {
        state.whale_btc_pool.save(deps.storage, &whale_btc_pool)?;
    }

    Ok(Response::new().add_attribute("action", "erishub/update_config"))
}
