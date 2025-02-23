#![allow(clippy::mutable_key_type)]

use anyhow::{anyhow, Result};
use gw_config::ContractsCellDep;
use gw_mem_pool::{custodian::sum_withdrawals, withdrawal::Generator};
use gw_types::core::Timepoint;
use gw_types::h256::*;
use gw_types::offchain::CompatibleFinalizedTimepoint;
use gw_types::packed::RollupConfig;
use gw_types::{
    bytes::Bytes,
    core::{DepType, ScriptHashType},
    offchain::{global_state_from_slice, CellInfo, CollectedCustodianCells, InputCellInfo},
    packed::{
        CellDep, CellInput, CellOutput, CustodianLockArgs, DepositLockArgs, L2Block, Script,
        UnlockWithdrawalViaFinalize, UnlockWithdrawalViaRevert, UnlockWithdrawalWitness,
        UnlockWithdrawalWitnessUnion, WithdrawalRequestExtra, WitnessArgs,
    },
    prelude::*,
};
use gw_utils::withdrawal::parse_lock_args;
use gw_utils::RollupContext;
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct GeneratedWithdrawals {
    pub deps: Vec<CellDep>,
    pub inputs: Vec<InputCellInfo>,
    pub outputs: Vec<(CellOutput, Bytes)>,
}

// Note: custodian lock search rollup cell in inputs
pub fn generate(
    rollup_context: &RollupContext,
    mut finalized_custodians: CollectedCustodianCells,
    block: &L2Block,
    contracts_dep: &ContractsCellDep,
    withdrawal_extras: &HashMap<H256, WithdrawalRequestExtra>,
) -> Result<Option<GeneratedWithdrawals>> {
    if block.withdrawals().is_empty() && finalized_custodians.cells_info.len() <= 1 {
        return Ok(None);
    }
    log::debug!("custodian inputs {:?}", finalized_custodians);

    let cells_info = std::mem::take(&mut finalized_custodians.cells_info);
    let cusotidan_sudt_is_empty = finalized_custodians.sudt.is_empty();

    let total_withdrawal_amount = sum_withdrawals(block.withdrawals().into_iter());
    let mut generator = Generator::new(rollup_context, finalized_custodians.into());
    for req in block.withdrawals().into_iter() {
        let req_extra = match withdrawal_extras.get(&req.hash()) {
            Some(req_extra) => req_extra.to_owned(),
            None => WithdrawalRequestExtra::new_builder().request(req).build(),
        };
        generator
            .include_and_verify(&req_extra, block)
            .map_err(|err| anyhow!("unexpected withdrawal err {}", err))?
    }
    log::debug!("included withdrawals {}", generator.withdrawals().len());

    let custodian_lock_dep = contracts_dep.custodian_cell_lock.clone();
    let sudt_type_dep = contracts_dep.l1_sudt_type.clone();
    let mut cell_deps = vec![custodian_lock_dep.into()];
    if !total_withdrawal_amount.sudt.is_empty() || !cusotidan_sudt_is_empty {
        cell_deps.push(sudt_type_dep.into());
    }

    let custodian_inputs = cells_info.into_iter().map(|cell| {
        let input = CellInput::new_builder()
            .previous_output(cell.out_point.clone())
            .build();
        InputCellInfo { input, cell }
    });

    let generated_withdrawals = GeneratedWithdrawals {
        deps: cell_deps,
        inputs: custodian_inputs.collect(),
        outputs: generator.finish(),
    };

    Ok(Some(generated_withdrawals))
}

pub struct RevertedWithdrawals {
    pub deps: Vec<CellDep>,
    pub inputs: Vec<InputCellInfo>,
    pub witness_args: Vec<WitnessArgs>,
    pub outputs: Vec<(CellOutput, Bytes)>,
}

pub fn revert(
    rollup_context: &RollupContext,
    contracts_dep: &ContractsCellDep,
    withdrawal_cells: Vec<CellInfo>,
) -> Result<Option<RevertedWithdrawals>> {
    if withdrawal_cells.is_empty() {
        return Ok(None);
    }

    let mut withdrawal_inputs = vec![];
    let mut withdrawal_witness = vec![];
    let mut custodian_outputs = vec![];

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unexpected timestamp")
        .as_millis() as u64;

    // We use timestamp plus idx and rollup_type_hash to create different custodian lock
    // hash for every reverted withdrawal input. Withdrawal lock use custodian lock hash to
    // index corresponding custodian output.
    // NOTE: These locks must also be different from custodian change cells created by
    // withdrawal requests processing.
    let rollup_type_hash = rollup_context.rollup_script_hash.as_slice().iter();
    for (idx, withdrawal) in withdrawal_cells.into_iter().enumerate() {
        let custodian_lock = {
            let deposit_lock_args = DepositLockArgs::new_builder()
                .owner_lock_hash(rollup_context.rollup_script_hash.pack())
                .cancel_timeout((idx as u64 + timestamp).pack())
                .build();

            let custodian_lock_args = CustodianLockArgs::new_builder()
                .deposit_lock_args(deposit_lock_args)
                .build();

            let lock_args: Bytes = rollup_type_hash
                .clone()
                .chain(custodian_lock_args.as_slice().iter())
                .cloned()
                .collect();

            Script::new_builder()
                .code_hash(rollup_context.rollup_config.custodian_script_type_hash())
                .hash_type(ScriptHashType::Type.into())
                .args(lock_args.pack())
                .build()
        };

        let custodian_output = {
            let output_builder = withdrawal.output.clone().as_builder();
            output_builder.lock(custodian_lock.clone()).build()
        };

        let withdrawal_input = {
            let input = CellInput::new_builder()
                .previous_output(withdrawal.out_point.clone())
                .build();

            InputCellInfo {
                input,
                cell: withdrawal.clone(),
            }
        };

        let unlock_withdrawal_witness = {
            let unlock_withdrawal_via_revert = UnlockWithdrawalViaRevert::new_builder()
                .custodian_lock_hash(custodian_lock.hash().pack())
                .build();

            UnlockWithdrawalWitness::new_builder()
                .set(UnlockWithdrawalWitnessUnion::UnlockWithdrawalViaRevert(
                    unlock_withdrawal_via_revert,
                ))
                .build()
        };
        let withdrawal_witness_args = WitnessArgs::new_builder()
            .lock(Some(unlock_withdrawal_witness.as_bytes()).pack())
            .build();

        withdrawal_inputs.push(withdrawal_input);
        withdrawal_witness.push(withdrawal_witness_args);
        custodian_outputs.push((custodian_output, withdrawal.data.clone()));
    }

    let withdrawal_lock_dep = contracts_dep.withdrawal_cell_lock.clone();
    let sudt_type_dep = contracts_dep.l1_sudt_type.clone();
    let mut cell_deps = vec![withdrawal_lock_dep.into()];
    if withdrawal_inputs
        .iter()
        .any(|info| info.cell.output.type_().to_opt().is_some())
    {
        cell_deps.push(sudt_type_dep.into())
    }

    Ok(Some(RevertedWithdrawals {
        deps: cell_deps,
        inputs: withdrawal_inputs,
        outputs: custodian_outputs,
        witness_args: withdrawal_witness,
    }))
}

#[derive(Debug)]
pub struct UnlockedWithdrawals {
    pub deps: Vec<CellDep>,
    pub inputs: Vec<InputCellInfo>,
    pub witness_args: Vec<WitnessArgs>,
    pub outputs: Vec<(CellOutput, Bytes)>,
}

pub fn unlock_to_owner(
    rollup_cell: CellInfo,
    rollup_config: &RollupConfig,
    contracts_dep: &ContractsCellDep,
    withdrawal_cells: Vec<CellInfo>,
    global_state_since: u64,
) -> Result<Option<UnlockedWithdrawals>> {
    if withdrawal_cells.is_empty() {
        return Ok(None);
    }

    let mut withdrawal_inputs = vec![];
    let mut withdrawal_witness = vec![];
    let mut unlocked_to_owner_outputs = vec![];

    let unlock_via_finalize_witness = {
        let unlock_args = UnlockWithdrawalViaFinalize::new_builder().build();
        let unlock_witness = UnlockWithdrawalWitness::new_builder()
            .set(UnlockWithdrawalWitnessUnion::UnlockWithdrawalViaFinalize(
                unlock_args,
            ))
            .build();
        WitnessArgs::new_builder()
            .lock(Some(unlock_witness.as_bytes()).pack())
            .build()
    };

    let global_state = global_state_from_slice(&rollup_cell.data)?;
    let compatible_finalized_timepoint = CompatibleFinalizedTimepoint::from_global_state(
        &global_state,
        rollup_config.finality_blocks().unpack(),
    );
    let l1_sudt_script_hash = rollup_config.l1_sudt_script_type_hash();
    let mut if_exist_legacy_withdrawal_cells = false;
    for withdrawal_cell in withdrawal_cells {
        // Double check
        if let Err(err) = gw_rpc_client::withdrawal::verify_unlockable_to_owner(
            &withdrawal_cell,
            &compatible_finalized_timepoint,
            &l1_sudt_script_hash,
        ) {
            log::error!("[unlock withdrawal] unexpected verify failed {}", err);
            continue;
        }

        if !if_exist_legacy_withdrawal_cells {
            if_exist_legacy_withdrawal_cells = is_legacy_finality_withdrawal_cell(&withdrawal_cell);
        }

        let owner_lock = {
            let args: Bytes = withdrawal_cell.output.lock().args().unpack();
            match gw_utils::withdrawal::parse_lock_args(&args) {
                Ok(parsed) => parsed.owner_lock,
                Err(_) => {
                    log::error!("[unlock withdrawal] impossible, already pass verify_unlockable_to_owner above");
                    continue;
                }
            }
        };

        let withdrawal_input = {
            let input = CellInput::new_builder()
                .previous_output(withdrawal_cell.out_point.clone())
                .since(global_state_since.pack())
                .build();

            InputCellInfo {
                input,
                cell: withdrawal_cell.clone(),
            }
        };

        // Switch to owner lock
        let output = withdrawal_cell.output.as_builder().lock(owner_lock).build();

        withdrawal_inputs.push(withdrawal_input);
        withdrawal_witness.push(unlock_via_finalize_witness.clone());
        unlocked_to_owner_outputs.push((output, withdrawal_cell.data));
    }

    if withdrawal_inputs.is_empty() {
        return Ok(None);
    }

    let rollup_dep = CellDep::new_builder()
        .out_point(rollup_cell.out_point)
        .dep_type(DepType::Code.into())
        .build();
    let rollup_config_dep = contracts_dep.rollup_config.clone();
    let withdrawal_lock_dep = contracts_dep.withdrawal_cell_lock.clone();
    let sudt_type_dep = contracts_dep.l1_sudt_type.clone();

    let mut cell_deps = if if_exist_legacy_withdrawal_cells {
        // Some withdrawal cells were born at legacy version, withdrawal_lock_script checks finality of withdrawal
        // cells by comparing with GlobalState.last_finalized_timepoint, so rollup_dep and
        // rollup_config_dep are required
        vec![
            rollup_dep,
            rollup_config_dep.into(),
            withdrawal_lock_dep.into(),
        ]
    } else {
        // All withdrawal cells were born at v2, withdrawal_lock_script checks finality of withdrawal
        // cells by comparing with `since`.
        vec![withdrawal_lock_dep.into()]
    };

    if unlocked_to_owner_outputs
        .iter()
        .any(|output| output.0.type_().to_opt().is_some())
    {
        cell_deps.push(sudt_type_dep.into())
    }

    Ok(Some(UnlockedWithdrawals {
        deps: cell_deps,
        inputs: withdrawal_inputs,
        witness_args: withdrawal_witness,
        outputs: unlocked_to_owner_outputs,
    }))
}

fn is_legacy_finality_withdrawal_cell(withdrawal_cell: &CellInfo) -> bool {
    let withdrawal_lock_args = parse_lock_args(&withdrawal_cell.output.lock().args().raw_data())
        .expect("parse withdrawal lock args");
    match Timepoint::from_full_value(
        withdrawal_lock_args
            .lock_args
            .withdrawal_finalized_timepoint()
            .unpack(),
    ) {
        Timepoint::BlockNumber(_) => true,
        Timepoint::Timestamp(_) => false,
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::iter::FromIterator;

    use crate::utils::global_state_last_finalized_timepoint_to_since;
    use crate::withdrawal::generate;
    use gw_config::{ContractsCellDep, ForkConfig};
    use gw_types::core::{DepType, ScriptHashType, Timepoint};
    use gw_types::h256::*;
    use gw_types::offchain::{
        CellInfo, CollectedCustodianCells, CompatibleFinalizedTimepoint, InputCellInfo,
    };
    use gw_types::packed::{
        BlockMerkleState, CellDep, CellInput, CellOutput, GlobalState, L2Block, OutPoint,
        RawL2Block, RawWithdrawalRequest, RollupConfig, Script, UnlockWithdrawalViaFinalize,
        UnlockWithdrawalWitness, UnlockWithdrawalWitnessUnion, WithdrawalLockArgs,
        WithdrawalRequest, WithdrawalRequestExtra, WitnessArgs,
    };
    use gw_types::prelude::{Builder, Entity, Pack, PackVec, Unpack};
    use gw_utils::{global_state_finalized_timepoint, RollupContext};

    use super::unlock_to_owner;

    #[test]
    fn test_withdrawal_cell_generate() {
        let rollup_context = RollupContext {
            rollup_script_hash: H256::from_u32(1),
            rollup_config: RollupConfig::new_builder()
                .withdrawal_script_type_hash(H256::from_u32(100).pack())
                .finality_blocks(1u64.pack())
                .build(),
            ..Default::default()
        };

        let sudt_script = Script::new_builder()
            .code_hash(H256::from_u32(2).pack())
            .hash_type(ScriptHashType::Type.into())
            .args(vec![3u8; 32].pack())
            .build();

        let finalized_custodians = CollectedCustodianCells {
            cells_info: vec![CellInfo::default()],
            capacity: u64::MAX as u128,
            sudt: HashMap::from_iter([(sudt_script.hash(), (u128::MAX, sudt_script.clone()))]),
        };

        let owner_lock = Script::new_builder()
            .code_hash(H256::from_u32(4).pack())
            .args(vec![5; 32].pack())
            .build();

        let withdrawal = {
            let fee = 50u128;
            let raw = RawWithdrawalRequest::new_builder()
                .nonce(1u32.pack())
                .capacity((500 * 10u64.pow(8)).pack())
                .amount(20u128.pack())
                .sudt_script_hash(sudt_script.hash().pack())
                .account_script_hash(H256::from_u32(10).pack())
                .owner_lock_hash(owner_lock.hash().pack())
                .fee(fee.pack())
                .build();
            WithdrawalRequest::new_builder()
                .raw(raw)
                .signature(vec![6u8; 65].pack())
                .build()
        };

        let raw_block = RawL2Block::new_builder().number(1000u64.pack()).build();
        let block = L2Block::new_builder()
            .raw(raw_block)
            .withdrawals(vec![withdrawal.clone()].pack())
            .build();

        let contracts_dep = ContractsCellDep::default();

        // ## With owner lock
        let withdrawal_extra = WithdrawalRequestExtra::new_builder()
            .request(withdrawal.clone())
            .owner_lock(owner_lock)
            .build();
        let withdrawal_extras = HashMap::from_iter([(withdrawal.hash(), withdrawal_extra.clone())]);

        let generated = generate(
            &rollup_context,
            finalized_custodians,
            &block,
            &contracts_dep,
            &withdrawal_extras,
        )
        .unwrap();
        let (output, data) = generated.unwrap().outputs.first().unwrap().to_owned();

        let block_timepoint = Timepoint::from_block_number(block.raw().number().unpack());
        let (expected_output, expected_data) = gw_generator::utils::build_withdrawal_cell_output(
            &rollup_context,
            &withdrawal_extra,
            &block.hash(),
            &block_timepoint,
            Some(sudt_script.clone()),
        )
        .unwrap();

        assert_eq!(expected_output.to_string(), output.to_string());
        assert_eq!(expected_data, data);

        // Check our generate withdrawal can be queried and unlocked to owner
        let info = CellInfo {
            output,
            data,
            ..Default::default()
        };
        // Make sure the withdrawal is finalized for `global_state`
        let compatible_finalized_timepoint = CompatibleFinalizedTimepoint::from_block_number(
            block.raw().number().unpack() + rollup_context.rollup_config.finality_blocks().unpack(),
            rollup_context.rollup_config.finality_blocks().unpack(),
        );
        gw_rpc_client::withdrawal::verify_unlockable_to_owner(
            &info,
            &compatible_finalized_timepoint,
            &sudt_script.code_hash(),
        )
        .expect("pass verification");
    }

    #[test]
    fn test_unlock_to_owner_v1() {
        // Output should only change lock to owner lock
        let last_finalized_timepoint = Timepoint::from_block_number(100);
        let global_state = GlobalState::new_builder()
            .last_finalized_timepoint(last_finalized_timepoint.full_value().pack())
            .build();

        let rollup_type = Script::new_builder()
            .code_hash(H256::from_u32(1).pack())
            .build();

        let rollup_cell = CellInfo {
            data: global_state.as_bytes(),
            out_point: OutPoint::new_builder()
                .tx_hash(H256::from_u32(2).pack())
                .build(),
            output: CellOutput::new_builder()
                .type_(Some(rollup_type.clone()).pack())
                .build(),
        };

        let sudt_script = Script::new_builder()
            .code_hash(H256::from_u32(3).pack())
            .hash_type(ScriptHashType::Type.into())
            .args(vec![4u8; 32].pack())
            .build();

        let rollup_context = RollupContext {
            rollup_script_hash: rollup_type.hash(),
            rollup_config: RollupConfig::new_builder()
                .withdrawal_script_type_hash(H256::from_u32(5).pack())
                .l1_sudt_script_type_hash(sudt_script.code_hash())
                .finality_blocks(1u64.pack())
                .build(),
            ..Default::default()
        };

        let contracts_dep = {
            let withdrawal_out_point = OutPoint::new_builder()
                .tx_hash(H256::from_u32(6).pack())
                .build();
            let l1_sudt_out_point = OutPoint::new_builder()
                .tx_hash(H256::from_u32(7).pack())
                .build();

            ContractsCellDep {
                withdrawal_cell_lock: CellDep::new_builder()
                    .out_point(withdrawal_out_point)
                    .build()
                    .into(),
                l1_sudt_type: CellDep::new_builder()
                    .out_point(l1_sudt_out_point)
                    .build()
                    .into(),
                ..Default::default()
            }
        };

        let owner_lock = Script::new_builder()
            .code_hash(H256::from_u32(8).pack())
            .hash_type(ScriptHashType::Type.into())
            .args(vec![9u8; 32].pack())
            .build();

        let withdrawal_without_owner_lock = {
            let lock_args = WithdrawalLockArgs::new_builder()
                .owner_lock_hash(owner_lock.hash().pack())
                .withdrawal_finalized_timepoint(last_finalized_timepoint.full_value().pack())
                .build();

            let mut args = rollup_type.hash().to_vec();
            args.extend_from_slice(&lock_args.as_bytes());

            let lock = Script::new_builder().args(args.pack()).build();
            CellInfo {
                output: CellOutput::new_builder().lock(lock).build(),
                ..Default::default()
            }
        };

        let withdrawal_with_owner_lock = {
            let lock_args = WithdrawalLockArgs::new_builder()
                .owner_lock_hash(owner_lock.hash().pack())
                .withdrawal_finalized_timepoint(last_finalized_timepoint.full_value().pack())
                .build();

            let mut args = rollup_type.hash().to_vec();
            args.extend_from_slice(&lock_args.as_bytes());
            args.extend_from_slice(&(owner_lock.as_bytes().len() as u32).to_be_bytes());
            args.extend_from_slice(&owner_lock.as_bytes());

            let lock = Script::new_builder().args(args.pack()).build();
            CellInfo {
                output: CellOutput::new_builder()
                    .type_(Some(sudt_script).pack())
                    .lock(lock)
                    .build(),
                data: 100u128.pack().as_bytes(),
                ..Default::default()
            }
        };

        let global_state_since = global_state_last_finalized_timepoint_to_since(&global_state);
        let unlocked = unlock_to_owner(
            rollup_cell.clone(),
            &rollup_context.rollup_config,
            &contracts_dep,
            vec![
                withdrawal_without_owner_lock,
                withdrawal_with_owner_lock.clone(),
            ],
            global_state_since,
        )
        .expect("unlock")
        .expect("some unlocked");

        assert_eq!(unlocked.inputs.len(), 1, "skip one without owner lock");
        assert_eq!(unlocked.outputs.len(), 1);
        assert_eq!(unlocked.witness_args.len(), 1);

        let expected_output = {
            let output = withdrawal_with_owner_lock.output.clone().as_builder();
            output.lock(owner_lock).build()
        };

        let (output, data) = unlocked.outputs.first().unwrap().to_owned();
        assert_eq!(expected_output.as_slice(), output.as_slice());
        assert_eq!(withdrawal_with_owner_lock.data, data);

        let expected_input = {
            let input = CellInput::new_builder()
                .previous_output(withdrawal_with_owner_lock.out_point.clone())
                .build();

            InputCellInfo {
                input,
                cell: withdrawal_with_owner_lock,
            }
        };
        let input = unlocked.inputs.first().unwrap().to_owned();
        assert_eq!(expected_input.input.as_slice(), input.input.as_slice());
        assert_eq!(
            expected_input.cell.output.as_slice(),
            input.cell.output.as_slice()
        );
        assert_eq!(
            expected_input.cell.out_point.as_slice(),
            input.cell.out_point.as_slice()
        );
        assert_eq!(expected_input.cell.data, input.cell.data);

        let expected_witness = {
            let unlock_args = UnlockWithdrawalViaFinalize::new_builder().build();
            let unlock_witness = UnlockWithdrawalWitness::new_builder()
                .set(UnlockWithdrawalWitnessUnion::UnlockWithdrawalViaFinalize(
                    unlock_args,
                ))
                .build();
            WitnessArgs::new_builder()
                .lock(Some(unlock_witness.as_bytes()).pack())
                .build()
        };
        let witness = unlocked.witness_args.first().unwrap().to_owned();
        assert_eq!(expected_witness.as_slice(), witness.as_slice());

        assert_eq!(unlocked.deps.len(), 4);
        let rollup_dep = CellDep::new_builder()
            .out_point(rollup_cell.out_point)
            .dep_type(DepType::Code.into())
            .build();
        assert_eq!(
            unlocked.deps.first().unwrap().as_slice(),
            rollup_dep.as_slice()
        );
        assert_eq!(
            unlocked.deps.get(1).unwrap().as_slice(),
            CellDep::from(contracts_dep.rollup_config).as_slice(),
        );
        assert_eq!(
            unlocked.deps.get(2).unwrap().as_slice(),
            CellDep::from(contracts_dep.withdrawal_cell_lock).as_slice(),
        );
        assert_eq!(
            unlocked.deps.get(3).unwrap().as_slice(),
            CellDep::from(contracts_dep.l1_sudt_type).as_slice(),
        );
    }

    #[test]
    fn test_unlock_to_owner_finality() {
        const FINALITY_BLOCKS: u64 = 10;
        const UPGRADE_GLOBAL_STATE_VERSION_TO_V2: u64 = 100;
        const BLOCK_TIMESTAMP: u64 = 1670000000000;

        let cases = vec![
            CaseParam {
                // v1 withdrawal, v1 global state, not finalized
                id: 0,
                finality_blocks: FINALITY_BLOCKS,
                upgrade_global_state_version_to_v2: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                block_timestamp: BLOCK_TIMESTAMP,
                block_number: UPGRADE_GLOBAL_STATE_VERSION_TO_V2 - 1,
                withdrawal_finalized_timepoint: Timepoint::BlockNumber(
                    UPGRADE_GLOBAL_STATE_VERSION_TO_V2 - FINALITY_BLOCKS,
                ),
                expected_result: Err(()),
            },
            CaseParam {
                // v1 withdrawal, v1 global state, finalized
                id: 1,
                finality_blocks: FINALITY_BLOCKS,
                upgrade_global_state_version_to_v2: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                block_timestamp: BLOCK_TIMESTAMP,
                block_number: UPGRADE_GLOBAL_STATE_VERSION_TO_V2 - 1,
                withdrawal_finalized_timepoint: Timepoint::BlockNumber(
                    UPGRADE_GLOBAL_STATE_VERSION_TO_V2 - 1 - FINALITY_BLOCKS,
                ),
                expected_result: Ok(()),
            },
            CaseParam {
                // v1 withdrawal, v2 global state, not finalized
                id: 2,
                finality_blocks: FINALITY_BLOCKS,
                upgrade_global_state_version_to_v2: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                block_timestamp: BLOCK_TIMESTAMP,
                block_number: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                withdrawal_finalized_timepoint: Timepoint::BlockNumber(
                    UPGRADE_GLOBAL_STATE_VERSION_TO_V2 + 1 - FINALITY_BLOCKS,
                ),
                expected_result: Err(()),
            },
            CaseParam {
                // v1 withdrawal, v2 global state, finalized
                id: 3,
                finality_blocks: FINALITY_BLOCKS,
                upgrade_global_state_version_to_v2: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                block_timestamp: BLOCK_TIMESTAMP,
                block_number: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                withdrawal_finalized_timepoint: Timepoint::BlockNumber(
                    UPGRADE_GLOBAL_STATE_VERSION_TO_V2 - FINALITY_BLOCKS,
                ),
                expected_result: Ok(()),
            },
            CaseParam {
                // v2 withdrawal, v2 global state, not finalized
                id: 4,
                finality_blocks: FINALITY_BLOCKS,
                upgrade_global_state_version_to_v2: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                block_timestamp: BLOCK_TIMESTAMP,
                block_number: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                withdrawal_finalized_timepoint: Timepoint::Timestamp(BLOCK_TIMESTAMP + 1),
                expected_result: Err(()),
            },
            CaseParam {
                // v2 withdrawal, v2 global state, finalized
                id: 5,
                finality_blocks: FINALITY_BLOCKS,
                upgrade_global_state_version_to_v2: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                block_timestamp: BLOCK_TIMESTAMP,
                block_number: UPGRADE_GLOBAL_STATE_VERSION_TO_V2,
                withdrawal_finalized_timepoint: Timepoint::Timestamp(BLOCK_TIMESTAMP),
                expected_result: Ok(()),
            },
        ];
        for case in cases {
            run_case(case);
        }

        #[derive(Debug)]
        struct CaseParam {
            #[allow(dead_code)]
            id: usize,
            finality_blocks: u64,
            upgrade_global_state_version_to_v2: u64,
            block_timestamp: u64,
            block_number: u64,
            withdrawal_finalized_timepoint: Timepoint,
            expected_result: Result<(), ()>,
        }

        fn run_case(case_param: CaseParam) {
            println!("case: {:?}", case_param);
            let CaseParam {
                id: _,
                finality_blocks,
                upgrade_global_state_version_to_v2,
                block_number,
                block_timestamp,
                withdrawal_finalized_timepoint,
                expected_result,
            } = case_param;

            let sudt_script = Script::new_builder()
                .code_hash(H256::from_u32(3).pack())
                .hash_type(ScriptHashType::Type.into())
                .args(vec![4u8; 32].pack())
                .build();
            let rollup_config = RollupConfig::new_builder()
                .l1_sudt_script_type_hash(sudt_script.code_hash())
                .finality_blocks(finality_blocks.pack())
                .build();
            let fork_config = ForkConfig {
                upgrade_global_state_version_to_v2: Some(upgrade_global_state_version_to_v2),
                ..Default::default()
            };
            let global_state = GlobalState::new_builder()
                .rollup_config_hash(rollup_config.hash().pack())
                .last_finalized_timepoint(
                    global_state_finalized_timepoint(
                        &rollup_config,
                        &fork_config,
                        block_number,
                        block_timestamp,
                    )
                    .full_value()
                    .pack(),
                )
                .block(
                    BlockMerkleState::new_builder()
                        .count((block_number + 1).pack())
                        .build(),
                )
                .build();
            let rollup_state_script = Script::new_builder()
                .code_hash(H256::from_u32(1).pack())
                .build();
            let rollup_state_cell = CellInfo {
                data: global_state.as_bytes(),
                out_point: OutPoint::new_builder()
                    .tx_hash(H256::from_u32(2).pack())
                    .build(),
                output: CellOutput::new_builder()
                    .type_(Some(rollup_state_script.clone()).pack())
                    .build(),
            };
            let rollup_context = RollupContext {
                rollup_script_hash: rollup_state_script.hash(),
                rollup_config: rollup_config.clone(),
                fork_config,
            };
            let contracts_dep = {
                ContractsCellDep {
                    withdrawal_cell_lock: CellDep::new_builder()
                        .out_point(
                            OutPoint::new_builder()
                                .tx_hash(H256::from_u32(6).pack())
                                .build(),
                        )
                        .build()
                        .into(),
                    l1_sudt_type: CellDep::new_builder()
                        .out_point(
                            OutPoint::new_builder()
                                .tx_hash(H256::from_u32(7).pack())
                                .build(),
                        )
                        .build()
                        .into(),
                    ..Default::default()
                }
            };

            // Build owner's lock script and withdrawal cell
            let owner_lock_script = Script::new_builder()
                .code_hash(H256::from_u32(8).pack())
                .hash_type(ScriptHashType::Type.into())
                .args(vec![9u8; 32].pack())
                .build();
            let withdrawal_lock_args = WithdrawalLockArgs::new_builder()
                .owner_lock_hash(owner_lock_script.hash().pack())
                .withdrawal_finalized_timepoint(withdrawal_finalized_timepoint.full_value().pack())
                .build();
            let withdrawal_cell = CellInfo {
                output: CellOutput::new_builder()
                    .type_(Some(sudt_script).pack())
                    .lock(
                        Script::new_builder()
                            .code_hash(rollup_config.withdrawal_script_type_hash())
                            .hash_type(ScriptHashType::Type.into())
                            .args({
                                let mut args = rollup_state_script.hash().to_vec();
                                args.extend_from_slice(&withdrawal_lock_args.as_bytes());
                                args.extend_from_slice(
                                    &(owner_lock_script.as_bytes().len() as u32).to_be_bytes(),
                                );
                                args.extend_from_slice(&owner_lock_script.as_bytes());
                                args.pack()
                            })
                            .build(),
                    )
                    .build(),
                data: 100u128.pack().as_bytes(),
                ..Default::default()
            };

            let unlocked = unlock_to_owner(
                rollup_state_cell,
                &rollup_context.rollup_config,
                &contracts_dep,
                vec![withdrawal_cell],
                global_state_last_finalized_timepoint_to_since(&global_state),
            )
            .expect("unlock");

            match expected_result {
                Ok(()) => {
                    assert!(unlocked.is_some());
                    let unlocked = unlocked.unwrap();
                    for input in unlocked.inputs.iter() {
                        assert_eq!(
                            input.input.since().unpack(),
                            global_state_last_finalized_timepoint_to_since(&global_state),
                        );
                    }
                }
                Err(()) => {
                    assert!(unlocked.is_none(), "actual unlocked: {:?}", unlocked);
                }
            }
        }
    }
}
