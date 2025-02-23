use anyhow::Result;
use ckb_types::prelude::{Builder, Entity};
use gw_rpc_client::indexer_client::CKBIndexerClient;
use gw_types::h256::*;
use gw_types::offchain::CompatibleFinalizedTimepoint;
use gw_types::{core::ScriptHashType, offchain::CustodianStat, packed::Script, prelude::Pack};

/// Query custodian ckb from ckb-indexer
pub async fn stat_custodian_cells(
    rpc_client: &CKBIndexerClient,
    rollup_type_hash: &H256,
    custodian_script_type_hash: &H256,
    min_capacity: Option<u64>,
    compatible_finalized_timepoint: &CompatibleFinalizedTimepoint,
) -> Result<CustodianStat> {
    let script = Script::new_builder()
        .code_hash(custodian_script_type_hash.pack())
        .hash_type(ScriptHashType::Type.into())
        .args(rollup_type_hash.as_slice().to_vec().pack())
        .build();
    rpc_client
        .stat_custodian_cells(script, min_capacity, compatible_finalized_timepoint)
        .await
}
