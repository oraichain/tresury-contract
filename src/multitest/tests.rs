use crate::msg::{CollectFeeRequirement, ExecuteMsg};
use crate::{state::DistributeTarget, ContractError};
use cosmwasm_std::testing::MockApi;
use cosmwasm_std::{
    coin, to_binary, Addr, Empty, Event, GovMsg, IbcMsg, IbcQuery, MemoryStorage, Uint128,
};
use cw20::{BalanceResponse, Cw20ExecuteMsg};
use cw_multi_test::{
    error, AcceptingModule, App, AppBuilder, BankKeeper, DistributionKeeper, Executor,
    FailingModule, Router, StakeKeeper, StargateAcceptingModule, StargateMsg, StargateQuery,
    WasmKeeper,
};
use oraiswap::asset::AssetInfo;
use oraiswap::router;

use super::contract_ping_pong_mock::MockPingPongContract;
use super::{
    contract::TreasuryContract,
    mock_cw20_contract::MockCw20Contract,
    mock_router_contract::{Cw20Hook, MockRouter},
};

pub type StargateAccpetingModuleApp = App<
    BankKeeper,
    MockApi,
    MemoryStorage,
    FailingModule<Empty, Empty, Empty>,
    WasmKeeper<Empty, Empty>,
    StakeKeeper,
    DistributionKeeper,
    FailingModule<IbcMsg, IbcQuery, Empty>,
    FailingModule<GovMsg, Empty, Empty>,
    AcceptingModule<StargateMsg, StargateQuery, Empty>,
>;

const INITAL_BALANCE: u128 = 1000000000000000000u128;

fn mock_app() -> (
    StargateAccpetingModuleApp,
    TreasuryContract,
    MockCw20Contract,
    MockPingPongContract,
    MockRouter,
) {
    let builder = AppBuilder::default();

    let mut app =
        builder
            .with_stargate(StargateAcceptingModule::new())
            .build(|router, _, storage| {
                // init for App Owner a lot of balances
                router
                    .bank
                    .init_balance(
                        storage,
                        &Addr::unchecked("owner"),
                        vec![coin(INITAL_BALANCE, "orai"), coin(INITAL_BALANCE, "atom")],
                    )
                    .unwrap();
            });

    let owner = Addr::unchecked("owner");
    let finance = Addr::unchecked("finance");
    let cw20 =
        MockCw20Contract::instantiate(&mut app, &owner, &owner, Uint128::from(INITAL_BALANCE))
            .unwrap();

    let ping_pong = MockPingPongContract::instantiate(&mut app, &owner);
    let not_owner = Addr::unchecked("not_owner");
    let usdc = MockCw20Contract::instantiate(
        &mut app,
        &not_owner,
        &not_owner,
        Uint128::from(INITAL_BALANCE * 2),
    )
    .unwrap();

    let router = MockRouter::instantiate(&mut app, &owner, usdc.addr().clone());

    let treasury = TreasuryContract::instantiate(
        &mut app,
        &owner,
        &owner,
        cw20.addr(),
        Some(owner.clone().into()),
        router.addr(),
        vec![
            DistributeTarget {
                weight: 40,
                addr: ping_pong.addr().clone(),
                msg_hook: Some(to_binary(&Cw20Hook::Ping {}).unwrap()),
            },
            DistributeTarget {
                weight: 60,
                addr: finance,
                msg_hook: None,
            },
        ],
    )
    .unwrap();

    usdc.transfer(
        &mut app,
        &not_owner,
        treasury.addr(),
        Uint128::from(INITAL_BALANCE * 2),
    );

    (app, treasury, cw20, ping_pong, router)
}

#[test]
fn test_distribute_happy_path() {
    let owner = Addr::unchecked("owner");
    let finance = Addr::unchecked("finance");
    let distribute_amount = Uint128::from(100u64);
    let (mut app, treasury, cw20, ping_pong, router) = mock_app();
    let owner_balance: BalanceResponse = cw20.query_balance(&app, &owner);

    println!("owner balance: {:?}", owner_balance);

    cw20.transfer(
        &mut app,
        &owner,
        &Addr::from(treasury.clone()),
        Uint128::from(100u64),
    );

    let treasury_balance: BalanceResponse = cw20.query_balance(&app, treasury.addr());
    assert_eq!(treasury_balance.balance, distribute_amount);

    let res = treasury
        .distribute_token(&owner, &mut app, distribute_amount)
        .unwrap();

    let ping_event = res
        .events
        .into_iter()
        .filter(|event| event.ty == "wasm" && event.attributes[1].value == "ping")
        .collect::<Vec<Event>>();

    // assert the ping event is emitted
    assert!(!ping_event.is_empty());

    let treasury_balance: BalanceResponse = cw20.query_balance(&app, treasury.addr());
    assert_eq!(treasury_balance.balance, Uint128::zero());
    let ping_pong_balance: BalanceResponse = cw20.query_balance(&app, ping_pong.addr());
    assert_eq!(ping_pong_balance.balance, Uint128::from(40u128));
    let finance: BalanceResponse = cw20.query_balance(&app, &finance);
    assert_eq!(finance.balance, Uint128::from(60u128));
}

#[test]
fn test_exceed_balance_distribute() {
    // arrange
    let owner = Addr::unchecked("owner");
    let _finance = Addr::unchecked("finance");
    let distribute_amount = Uint128::from(100u64);

    let (mut app, treasury, cw20, _ping_pong, router) = mock_app();
    let owner_balance: BalanceResponse = cw20.query_balance(&app, &owner);

    println!("owner balance: {:?}", owner_balance);

    // act
    cw20.transfer(
        &mut app,
        &owner,
        &Addr::from(treasury.clone()),
        Uint128::from(100u64),
    );

    let err = treasury
        .distribute_token(
            &owner,
            &mut app,
            distribute_amount.checked_add(Uint128::from(1u64)).unwrap(),
        )
        .unwrap_err();

    // assert
    assert_eq!(err, ContractError::ExceedContractBalance {});
}

#[test]
fn test_collect_fees_balance_distribute() {
    // arrange
    let owner = Addr::unchecked("owner");
    let _finance = Addr::unchecked("finance");
    let (mut app, treasury, cw20, _ping_pong, _router) = mock_app();

    app.execute_contract(
        owner.clone(),
        cw20.addr().clone(),
        &Cw20ExecuteMsg::IncreaseAllowance {
            spender: treasury.addr().to_string(),
            amount: Uint128::from(1000000000000000000u128),
            expires: None,
        },
        &[],
    )
    .unwrap();

    // send all token of approver to treasury, but leave 1 token for fee  = authz.MsgExec(bank.MsgSend)
    app.send_tokens(
        owner.clone(),
        treasury.addr().clone(),
        &[coin(999999999999000000, "orai")],
    )
    .unwrap();

    //act
    let response = app
        .execute_contract(
            owner.clone(),
            treasury.addr().clone(),
            &ExecuteMsg::CollectFees {
                collect_fee_requirements: vec![
                    CollectFeeRequirement {
                        asset: AssetInfo::NativeToken {
                            denom: "orai".into(),
                        },
                        minimum_receive: None,
                    },
                    CollectFeeRequirement {
                        asset: AssetInfo::Token {
                            contract_addr: cw20.addr().clone(),
                        },
                        minimum_receive: None,
                    },
                ],
            },
            &[],
        )
        .unwrap();
}
