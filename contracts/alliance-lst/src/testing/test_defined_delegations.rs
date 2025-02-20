use std::str::FromStr;

use cosmwasm_std::testing::{mock_env, mock_info, MockApi, MockStorage, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{coin, Addr, Coin, Decimal, OwnedDeps, StdError, SubMsg, Uint128};
use eris_chain_adapter::types::CustomQueryType;

use super::custom_querier::CustomQuerier;
use super::helpers::{mock_dependencies, mock_env_at_timestamp, query_helper};
use crate::contract::{execute, instantiate};
use crate::error::ContractError;
use crate::state::State;
use crate::testing::helpers::{
    chain_test, check_received_coin, get_stake_full_denom, BTC_DENOM, MOCK_UTOKEN, WHALE_BTC_POOL,
    WHALE_DENOM,
};
use crate::types::{Delegation, Redelegation};
use eris::alliance_lst::{AllianceStakeToken, ExecuteMsg, InstantiateMsg, QueryMsg};
use eris::governance_helper::{EPOCH_START, WEEK};
use eris::hub::{
    ConfigResponse, DelegationStrategy, FeeConfig, StateResponse, WantedDelegationsResponse,
    WantedDelegationsShare,
};
use eris_chain_shared::chain_trait::ChainInterface;

//--------------------------------------------------------------------------------------------------
// Test setup
//--------------------------------------------------------------------------------------------------

pub const STAKE_DENOM: &str = "factory/cosmos2contract/stake";

fn setup_test() -> OwnedDeps<MockStorage, MockApi, CustomQuerier, CustomQueryType> {
    let mut deps = mock_dependencies();

    let res = instantiate(
        deps.as_mut(),
        mock_env_at_timestamp(EPOCH_START),
        mock_info("deployer", &[]),
        InstantiateMsg {
            owner: "owner".to_string(),
            utoken: MOCK_UTOKEN.to_string(),
            denom: "stake".to_string(),
            epoch_period: 259200,   // 3 * 24 * 60 * 60 = 3 days
            unbond_period: 1814400, // 21 * 24 * 60 * 60 = 21 days
            protocol_fee_contract: "fee".to_string(),
            protocol_reward_fee: Decimal::from_ratio(1u128, 100u128),
            operator: "operator".to_string(),
            delegation_strategy: Some(eris::hub::DelegationStrategy::Defined {
                shares_bps: vec![("alice".into(), 6000), ("bob".into(), 4000)],
            }),
            validator_proxy: "proxy".to_string(),
            whale_btc_pool: WHALE_BTC_POOL.to_string(),
            btc_denom: BTC_DENOM.to_string(),
            whale_denom: WHALE_DENOM.to_string(),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0].msg,
        chain_test().create_denom_msg(get_stake_full_denom(), "stake".to_string())
    );

    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(EPOCH_START + WEEK),
        mock_info("owner", &[Coin::new(1000000, MOCK_UTOKEN)]),
        ExecuteMsg::TuneDelegations {},
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    let state = State::default();

    assert_eq!(
        state.delegation_goal.load(deps.as_ref().storage).unwrap(),
        WantedDelegationsShare {
            tune_time: EPOCH_START + WEEK,
            tune_period: 1,
            shares: vec![
                ("alice".into(), Decimal::from_str("0.6").unwrap()),
                ("bob".into(), Decimal::from_str("0.4").unwrap())
            ]
        }
    );

    let res: WantedDelegationsResponse =
        query_helper(deps.as_ref(), QueryMsg::WantedDelegations {});

    assert_eq!(
        res,
        WantedDelegationsResponse {
            tune_time_period: Some((EPOCH_START + WEEK, 1)),
            // nothing bonded yet
            delegations: vec![("alice".into(), Uint128::zero()), ("bob".into(), Uint128::zero())]
        },
    );

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
            stake_token: STAKE_DENOM.to_string(),
            epoch_period: 259200,
            unbond_period: 1814400,
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            fee_config: FeeConfig {
                protocol_fee_contract: Addr::unchecked("fee"),
                protocol_reward_fee: Decimal::from_ratio(1u128, 100u128)
            },
            operator: "operator".to_string(),
            stages_preset: vec![],
            withdrawals_preset: vec![],
            allow_donations: false,
            delegation_strategy: DelegationStrategy::Defined {
                shares_bps: vec![("alice".into(), 6000), ("bob".into(), 4000)],
            },
            vote_operator: None,
            utoken: MOCK_UTOKEN.to_string()
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
}

#[test]
fn validate_update() {
    let mut deps = setup_test();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("owner", &[]),
        ExecuteMsg::UpdateConfig {
            protocol_fee_contract: None,
            protocol_reward_fee: None,
            operator: None,
            stages_preset: None,
            withdrawals_preset: None,
            allow_donations: None,
            delegation_strategy: Some(DelegationStrategy::Defined {
                shares_bps: vec![("abc".into(), 1000)],
            }),
            default_max_spread: None,
            epoch_period: None,
            unbond_period: None,
            validator_proxy: None,
            whale_denom: None,
            btc_denom: None,
            whale_btc_pool: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("validator abc not whitelisted").into());

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("owner", &[]),
        ExecuteMsg::UpdateConfig {
            protocol_fee_contract: None,
            protocol_reward_fee: None,
            operator: None,
            stages_preset: None,
            withdrawals_preset: None,
            allow_donations: None,
            delegation_strategy: Some(DelegationStrategy::Defined {
                shares_bps: vec![("alice".into(), 1000), ("alice".into(), 1000)],
            }),
            default_max_spread: None,

            epoch_period: None,
            unbond_period: None,
            validator_proxy: None,
            whale_denom: None,
            btc_denom: None,
            whale_btc_pool: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("validator alice duplicated").into());

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("owner", &[]),
        ExecuteMsg::UpdateConfig {
            protocol_fee_contract: None,
            protocol_reward_fee: None,
            operator: None,
            stages_preset: None,
            withdrawals_preset: None,
            allow_donations: None,
            delegation_strategy: Some(DelegationStrategy::Defined {
                shares_bps: vec![("alice".into(), 1000)],
            }),
            default_max_spread: None,
            epoch_period: None,
            unbond_period: None,
            validator_proxy: None,
            whale_denom: None,
            btc_denom: None,
            whale_btc_pool: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("sum of shares is not 10000").into());

    execute(
        deps.as_mut(),
        mock_env(),
        mock_info("owner", &[]),
        ExecuteMsg::UpdateConfig {
            protocol_fee_contract: None,
            protocol_reward_fee: None,
            operator: None,
            stages_preset: None,
            withdrawals_preset: None,
            allow_donations: None,
            delegation_strategy: Some(DelegationStrategy::Defined {
                shares_bps: vec![("alice".into(), 1000), ("charlie".into(), 9000)],
            }),
            default_max_spread: None,
            epoch_period: None,
            unbond_period: None,
            validator_proxy: None,
            whale_denom: None,
            btc_denom: None,
            whale_btc_pool: None,
        },
    )
    .unwrap();
}

#[test]
fn bonding() {
    let mut deps = setup_test();

    deps.querier.set_bank_balances(&[coin(1000100, MOCK_UTOKEN)]);

    // Bond when no delegation has been made
    // In this case, the full deposit goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1000000, MOCK_UTOKEN)]),
        ExecuteMsg::Bond {
            receiver: None,
        },
    )
    .unwrap();

    let mint_msgs = chain_test().create_mint_msgs(
        get_stake_full_denom(),
        Uint128::new(1000000),
        Addr::unchecked("user_1"),
    );
    assert_eq!(res.messages.len(), 2 + mint_msgs.len());

    let mut index = 0;
    assert_eq!(
        res.messages[0],
        SubMsg::new(
            Delegation::new("alice", 1000000, MOCK_UTOKEN)
                .to_cosmos_msg(MOCK_CONTRACT_ADDR.to_string())
        )
    );
    index += 1;
    for msg in mint_msgs {
        assert_eq!(res.messages[index].msg, msg);
        index += 1;
    }

    assert_eq!(res.messages[index], check_received_coin(100, 0));
    deps.querier.set_bank_balances(&[coin(12345 + 222, MOCK_UTOKEN)]);

    assert_eq!(
        State::default().stake_token.load(deps.as_ref().storage).unwrap(),
        AllianceStakeToken {
            utoken: MOCK_UTOKEN.to_string(),
            denom: STAKE_DENOM.to_string(),
            total_supply: Uint128::new(1000000),
            total_utoken_bonded: Uint128::new(1000000)
        }
    );

    // Update delegations to reflect the initial state after first bond
    deps.querier.set_staking_delegations(&[Delegation::new("alice", 1000000, MOCK_UTOKEN)]);

    // Bond when there are existing delegations
    // We'll simulate a 2.5% yield accumulation (1000000 * 1.025 = 1025000)
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 600000, MOCK_UTOKEN), // 60%
        Delegation::new("bob", 400000, MOCK_UTOKEN),   // 40%
    ]);

    // Second bond - should go to the validator with the lowest delegation relative to target
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_2", &[Coin::new(12043, MOCK_UTOKEN)]),
        ExecuteMsg::Bond {
            receiver: Some("user_3".to_string()),
        },
    )
    .unwrap();

    let mint_msgs = chain_test().create_mint_msgs(
        get_stake_full_denom(),
        Uint128::new(12043),
        Addr::unchecked("user_3"),
    );
    assert_eq!(res.messages.len(), 2 + mint_msgs.len());

    let mut index = 0;
    // Delegation goes to Alice as per original implementation
    assert_eq!(
        res.messages[0],
        SubMsg::new(
            Delegation::new("alice", 12043, MOCK_UTOKEN)
                .to_cosmos_msg(MOCK_CONTRACT_ADDR.to_string())
        )
    );
    index += 1;
    for msg in mint_msgs {
        assert_eq!(res.messages[index].msg, msg);
        index += 1;
    }

    assert_eq!(res.messages[index], check_received_coin(524, 0));

    // Update state to reflect both bonds
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 612043, MOCK_UTOKEN), // 600000 + 12043
        Delegation::new("bob", 400000, MOCK_UTOKEN),
    ]);

    let res: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(
        res,
        StateResponse {
            total_ustake: Uint128::new(1012043),
            total_utoken: Uint128::new(1012043),
            exchange_rate: Decimal::one(), // Should start at 1:1
            unlocked_coins: vec![],
            unbonding: Uint128::zero(),
            available: Uint128::new(12567),
            tvl_utoken: Uint128::new(1012043 + 12567),
        }
    );

    let res: WantedDelegationsResponse =
        query_helper(deps.as_ref(), QueryMsg::WantedDelegations {});
    assert_eq!(
        res,
        WantedDelegationsResponse {
            tune_time_period: Some((EPOCH_START + WEEK, 1)),
            delegations: vec![
                ("alice".into(), Uint128::new(607225)), // 60% of 1012043
                ("bob".into(), Uint128::new(404817))    // 40% of 1012043
            ]
        },
    );

    // Test unauthorized rebalance
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("alice", &[Coin::new(12345, MOCK_UTOKEN)]),
        ExecuteMsg::Rebalance {
            min_redelegation: None,
        },
    )
    .unwrap_err();
    assert_eq!(res, ContractError::Unauthorized {});

    // Test authorized rebalance
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("owner", &[Coin::new(12345, MOCK_UTOKEN)]),
        ExecuteMsg::Rebalance {
            min_redelegation: None,
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 2); // One redelegation and one received coin check

    assert_eq!(
        res.messages[0].msg,
        Redelegation {
            src: "alice".into(),
            dst: "bob".into(),
            amount: 404813, // Amount needed to reach target split
            denom: MOCK_UTOKEN.into()
        }
        .to_cosmos_msg(MOCK_CONTRACT_ADDR.to_string())
    );

    assert_eq!(res.messages[1], check_received_coin(12567, 0));
}
