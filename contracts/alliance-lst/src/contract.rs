use cosmwasm_std::{
    entry_point, to_json_binary, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdResult,
};
use cw2::set_contract_version;

use eris::alliance_lst::{ExecuteMsg, InstantiateMsg, QueryMsg};
use eris::hub::{CallbackMsg, MigrateMsg};
use eris_chain_adapter::types::CustomQueryType;

use crate::claim::exec_claim;
use crate::constants::{CONTRACT_NAME, CONTRACT_VERSION};
use crate::error::{ContractError, ContractResult};
use crate::state::State;
use crate::{execute, queries};

#[entry_point]
pub fn instantiate(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> ContractResult {
    execute::instantiate(deps, env, msg)
}

#[entry_point]
pub fn execute(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> ContractResult {
    let api = deps.api;
    match msg {
        ExecuteMsg::Bond {
            receiver,
        } => execute::bond(
            deps,
            env,
            receiver.map(|s| api.addr_validate(&s)).transpose()?.unwrap_or(info.sender),
            &info.funds,
            false,
        ),
        ExecuteMsg::Donate {} => execute::bond(deps, env, info.sender, &info.funds, true),
        ExecuteMsg::WithdrawUnbonded {
            receiver,
        } => execute::withdraw_unbonded(
            deps,
            env,
            info.sender.clone(),
            receiver.map(|s| api.addr_validate(&s)).transpose()?.unwrap_or(info.sender),
        ),
        ExecuteMsg::TransferOwnership {
            new_owner,
        } => execute::transfer_ownership(deps, info.sender, new_owner),
        ExecuteMsg::DropOwnershipProposal {} => execute::drop_ownership_proposal(deps, info.sender),
        ExecuteMsg::AcceptOwnership {} => execute::accept_ownership(deps, info.sender),
        ExecuteMsg::Harvest {
            validators,
            withdrawals,
            stages,
        } => execute::harvest(deps, env, validators, withdrawals, stages, info.sender),
        ExecuteMsg::TuneDelegations {} => execute::tune_delegations(deps, env, info.sender),
        ExecuteMsg::Rebalance {
            min_redelegation,
        } => execute::rebalance(deps, env, info.sender, min_redelegation),
        ExecuteMsg::Reconcile {} => execute::reconcile(deps, env),
        ExecuteMsg::CheckSlashing {
            delegations,
            state_total_utoken_bonded,
        } => {
            execute::check_slashing(deps, env, info.sender, delegations, state_total_utoken_bonded)
        },
        ExecuteMsg::SubmitBatch {
            undelegations,
        } => execute::submit_batch(deps, env, info.sender, undelegations),
        ExecuteMsg::Callback(callback_msg) => callback(deps, env, info, callback_msg),
        ExecuteMsg::UpdateConfig {
            protocol_fee_contract,
            protocol_reward_fee,
            operator,
            stages_preset,
            allow_donations,
            delegation_strategy,
            withdrawals_preset,
            default_max_spread,
            epoch_period,
            unbond_period,
            validator_proxy,
            whale_denom,
            btc_denom,
            whale_btc_pool,
        } => execute::update_config(
            deps,
            info.sender,
            protocol_fee_contract,
            protocol_reward_fee,
            operator,
            stages_preset,
            withdrawals_preset,
            allow_donations,
            delegation_strategy,
            default_max_spread,
            epoch_period,
            unbond_period,
            validator_proxy,
            whale_denom,
            btc_denom,
            whale_btc_pool,
        ),
        ExecuteMsg::QueueUnbond {
            receiver,
        } => {
            let state = State::default();
            let stake_token = state.stake_token.load(deps.storage)?;

            if info.funds.len() != 1 {
                return Err(ContractError::ExpectingSingleCoin {});
            }

            if info.funds[0].denom != stake_token.denom {
                return Err(ContractError::ExpectingAllianceStakeToken(
                    info.funds[0].denom.to_string(),
                ));
            }

            execute::queue_unbond(
                deps,
                env,
                api.addr_validate(&receiver.unwrap_or_else(|| info.sender.to_string()))?,
                info.funds[0].amount,
            )
        },
        ExecuteMsg::Claim {
            claims,
        } => exec_claim(deps, env, info, claims),
    }
}

fn callback(
    deps: DepsMut<CustomQueryType>,
    env: Env,
    info: MessageInfo,
    callback_msg: CallbackMsg,
) -> ContractResult {
    if env.contract.address != info.sender {
        return Err(ContractError::CallbackOnlyCalledByContract {});
    }

    match callback_msg {
        CallbackMsg::Reinvest {
            skip_fee,
        } => execute::reinvest(deps, env, skip_fee),
        CallbackMsg::WithdrawLps {
            withdrawals,
        } => execute::withdraw_lps(deps, env, withdrawals),
        CallbackMsg::SingleStageSwap {
            stage,
            index,
        } => execute::single_stage_swap(deps, env, stage, index),
        CallbackMsg::CheckReceivedCoin {
            snapshot,
            snapshot_stake,
        } => execute::callback_received_coins(deps, env, snapshot, snapshot_stake),
        CallbackMsg::ProvideLiquidity {} => execute::provide_liquidity_msg(&deps, &env),
        CallbackMsg::HalfSwapReward {} => execute::half_swap_reward_msg(&deps, &env),
    }
}

#[entry_point]
pub fn query(deps: Deps<CustomQueryType>, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&queries::config(deps)?),
        QueryMsg::State {} => to_json_binary(&queries::state(deps, env)?),
        QueryMsg::PendingBatch {} => to_json_binary(&queries::pending_batch(deps)?),
        QueryMsg::PreviousBatch(id) => to_json_binary(&queries::previous_batch(deps, id)?),
        QueryMsg::PreviousBatches {
            start_after,
            limit,
        } => to_json_binary(&queries::previous_batches(deps, start_after, limit)?),
        QueryMsg::UnbondRequestsByBatch {
            id,
            start_after,
            limit,
        } => to_json_binary(&queries::unbond_requests_by_batch(deps, id, start_after, limit)?),
        QueryMsg::UnbondRequestsByUser {
            user,
            start_after,
            limit,
        } => to_json_binary(&queries::unbond_requests_by_user(deps, user, start_after, limit)?),

        QueryMsg::UnbondRequestsByUserDetails {
            user,
            start_after,
            limit,
        } => to_json_binary(&queries::unbond_requests_by_user_details(
            deps,
            user,
            start_after,
            limit,
            env,
        )?),
        QueryMsg::WantedDelegations {} => to_json_binary(&queries::wanted_delegations(deps, env)?),
        QueryMsg::SimulateWantedDelegations {
            period,
        } => to_json_binary(&queries::simulate_wanted_delegations(deps, env, period)?),

        QueryMsg::ExchangeRates {
            start_after,
            limit,
        } => to_json_binary(&queries::query_exchange_rates(deps, env, start_after, limit)?),
        QueryMsg::Delegations {} => to_json_binary(&queries::delegations(deps, env)?),
        QueryMsg::SimulateUndelegations {} => {
            to_json_binary(&queries::simulate_undelegations(deps, env)?)
        },
    }
}

#[entry_point]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> ContractResult {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    Ok(Response::new()
        .add_attribute("new_contract_name", CONTRACT_NAME)
        .add_attribute("new_contract_version", CONTRACT_VERSION))
}
