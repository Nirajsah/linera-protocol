// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![cfg(with_wasm_runtime)]

use std::sync::Arc;

use linera_base::{
    data_types::{Amount, Blob, BlockHeight, OracleResponse, Timestamp},
    identifiers::{ChainDescription, ChainId},
};
use linera_execution::{
    test_utils::{create_dummy_user_application_description, SystemExecutionState},
    ExecutionRuntimeConfig, ExecutionRuntimeContext, Operation, OperationContext, Query,
    QueryContext, QueryOutcome, QueryResponse, ResourceControlPolicy, ResourceController,
    ResourceTracker, TransactionTracker, WasmContractModule, WasmRuntime, WasmServiceModule,
};
use linera_views::{context::Context as _, views::View};
use serde_json::json;
use test_case::test_case;

/// Test if the "counter" example application in `linera-sdk` compiled to a Wasm module can be
/// called correctly and consume the expected amount of fuel.
///
/// To update the bytecode files, run `linera-execution/update_wasm_fixtures.sh`.
#[cfg_attr(with_wasmer, test_case(WasmRuntime::Wasmer, 92_357; "wasmer"))]
#[cfg_attr(with_wasmer, test_case(WasmRuntime::WasmerWithSanitizer, 92_917; "wasmer_with_sanitizer"))]
#[cfg_attr(with_wasmtime, test_case(WasmRuntime::Wasmtime, 92_749; "wasmtime"))]
#[cfg_attr(with_wasmtime, test_case(WasmRuntime::WasmtimeWithSanitizer, 92_749; "wasmtime_with_sanitizer"))]
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn test_fuel_for_counter_wasm_application(
    wasm_runtime: WasmRuntime,
    expected_fuel: u64,
) -> anyhow::Result<()> {
    let state = SystemExecutionState {
        description: Some(ChainDescription::Root(0)),
        ..Default::default()
    };
    let mut view = state
        .into_view_with(ChainId::root(0), ExecutionRuntimeConfig::default())
        .await;
    let (app_desc, contract_blob, service_blob) = create_dummy_user_application_description(1);
    let app_id = From::from(&app_desc);
    let app_desc_blob_id = Blob::new_application_description(&app_desc).id();
    let contract_blob_id = contract_blob.id();
    let service_blob_id = service_blob.id();

    let contract =
        WasmContractModule::from_file("tests/fixtures/counter_contract.wasm", wasm_runtime).await?;
    view.context()
        .extra()
        .user_contracts()
        .insert(app_id, contract.into());

    let service =
        WasmServiceModule::from_file("tests/fixtures/counter_service.wasm", wasm_runtime).await?;
    view.context()
        .extra()
        .user_services()
        .insert(app_id, service.into());

    view.context()
        .extra()
        .add_blobs([
            contract_blob,
            service_blob,
            Blob::new_application_description(&app_desc),
        ])
        .await?;

    let context = OperationContext {
        chain_id: ChainId::root(0),
        height: BlockHeight(0),
        round: Some(0),
        index: Some(0),
        authenticated_signer: None,
        authenticated_caller_id: None,
    };
    let increments = [2_u64, 9, 7, 1000];
    let policy = ResourceControlPolicy {
        fuel_unit: Amount::from_attos(1),
        ..ResourceControlPolicy::default()
    };
    let amount = Amount::from_tokens(1);
    *view.system.balance.get_mut() = amount;
    let mut controller = ResourceController {
        policy: Arc::new(policy),
        tracker: ResourceTracker::default(),
        account: None,
    };

    for (index, increment) in increments.iter().enumerate() {
        let mut txn_tracker = TransactionTracker::new(
            0,
            0,
            Some(if index == 0 {
                vec![
                    OracleResponse::Blob(app_desc_blob_id),
                    OracleResponse::Blob(contract_blob_id),
                    OracleResponse::Blob(service_blob_id),
                ]
            } else {
                vec![]
            }),
        );
        view.execute_operation(
            context,
            Timestamp::from(0),
            Operation::user_without_abi(app_id, increment).unwrap(),
            &mut txn_tracker,
            &mut controller,
        )
        .await?;
        let txn_outcome = txn_tracker.into_outcome().unwrap();
        assert!(txn_outcome.outcomes.is_empty());
    }
    assert_eq!(controller.tracker.fuel, expected_fuel);
    assert_eq!(
        controller.with_state(&mut view).await?.balance().unwrap(),
        Amount::ONE
            .try_sub(Amount::from_attos(expected_fuel as u128))
            .unwrap()
    );

    let context = QueryContext {
        chain_id: ChainId::root(0),
        next_block_height: BlockHeight(0),
        local_time: Timestamp::from(0),
    };
    let mut service_runtime_endpoint = context.spawn_service_runtime_actor();
    let expected_value = async_graphql::Response::new(
        async_graphql::Value::from_json(json!({"value" : increments.into_iter().sum::<u64>()}))
            .unwrap(),
    );
    let request = async_graphql::Request::new("query { value }");
    let outcome = view
        .query_application(
            context,
            Query::user_without_abi(app_id, &request).unwrap(),
            Some(&mut service_runtime_endpoint),
        )
        .await?;
    let QueryOutcome {
        response: QueryResponse::User(serialized_value),
        operations,
    } = outcome
    else {
        panic!("unexpected response")
    };
    assert_eq!(
        serde_json::from_slice::<async_graphql::Response>(&serialized_value).unwrap(),
        expected_value
    );
    assert!(operations.is_empty());
    Ok(())
}
