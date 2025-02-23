#![allow(clippy::mutable_key_type)]
#![allow(clippy::unnecessary_unwrap)]
//! MemPool
//!
//! The mem pool will update txs & withdrawals 'instantly' by running background tasks.
//! So a user could query the tx receipt 'instantly'.
//! Since we already got the next block status, the block prodcuer would not need to execute
//! txs & withdrawals again.
//!

use anyhow::{anyhow, Context, Result};
use gw_common::{
    builtins::CKB_SUDT_ACCOUNT_ID, ckb_decimal::CKBCapacity, registry_address::RegistryAddress,
    state::State,
};
use gw_config::{MemBlockConfig, MemPoolConfig, NodeMode, SyscallCyclesConfig};
use gw_dynamic_config::manager::DynamicConfigManager;
use gw_generator::{
    error::TransactionError,
    generator::CyclesPool,
    traits::StateExt,
    verification::{transaction::TransactionVerifier, withdrawal::WithdrawalVerifier},
    ArcSwap, Generator,
};
use gw_store::{
    chain_view::ChainView,
    mem_pool_state::{self, MemPoolState, Shared},
    state::{traits::JournalDB, MemStateDB},
    traits::chain_store::ChainStore,
    transaction::StoreTransaction,
    Store,
};
use gw_traits::CodeStore;
use gw_types::packed::GlobalState;
use gw_types::{
    h256::*,
    offchain::{DepositInfo, FinalizedCustodianCapacity},
    packed::{
        AccountMerkleState, BlockInfo, L2Block, L2Transaction, NextMemBlock, Script, TxReceipt,
        WithdrawalKey, WithdrawalRequest, WithdrawalRequestExtra,
    },
    prelude::{Builder, Entity, Pack, PackVec, Unpack},
};
use gw_utils::calc_finalizing_range;
use gw_utils::local_cells::LocalCellsManager;
use std::{
    cmp::{max, min},
    collections::{HashMap, HashSet, VecDeque},
    iter::FromIterator,
    ops::Shr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::block_in_place;
use tracing::instrument;

use crate::{
    account_creator::{filter_new_address, AccountCreator},
    block_sync_server::BlockSyncServerState,
    mem_block::MemBlock,
    restore_manager::RestoreManager,
    traits::MemPoolProvider,
    types::EntryList,
    withdrawal::Generator as WithdrawalGenerator,
};

type StateDB = gw_store::state::MemStateDB;

#[derive(Debug, Default)]
pub struct OutputParam {
    pub retry_count: usize,
}

impl OutputParam {
    pub fn new(retry_count: usize) -> Self {
        OutputParam { retry_count }
    }
}

/// MemPool
pub struct MemPool {
    /// store
    store: Store,
    mem_pool_state: Arc<MemPoolState>,
    /// current tip
    current_tip: (H256, u64, GlobalState),
    /// generator instance
    generator: Arc<Generator>,
    /// pending queue, contains executable contents
    pending: HashMap<u32, EntryList>,
    /// memory block
    mem_block: MemBlock,
    /// Mem pool provider
    provider: Box<dyn MemPoolProvider + Send + Sync>,
    /// Pending deposits
    pending_deposits: Vec<DepositInfo>,
    /// Mem block save and restore
    restore_manager: RestoreManager,
    /// Restored txs to finalize
    pending_restored_tx_hashes: VecDeque<H256>,
    dynamic_config_manager: Arc<ArcSwap<DynamicConfigManager>>,
    sync_server: Option<Arc<std::sync::Mutex<BlockSyncServerState>>>,
    mem_block_config: MemBlockConfig,
    /// Cycles Pool
    cycles_pool: CyclesPool,
    /// Account creator
    account_creator: Option<AccountCreator>,
}

pub struct MemPoolCreateArgs {
    pub block_producer: RegistryAddress,
    pub store: Store,
    pub generator: Arc<Generator>,
    pub provider: Box<dyn MemPoolProvider + Send + Sync>,
    pub config: MemPoolConfig,
    pub node_mode: NodeMode,
    pub dynamic_config_manager: Arc<ArcSwap<DynamicConfigManager>>,
    pub sync_server: Option<Arc<std::sync::Mutex<BlockSyncServerState>>>,
    pub account_creator: Option<AccountCreator>,
}

impl Drop for MemPool {
    fn drop(&mut self) {
        log::info!("Saving mem block to {:?}", self.restore_manager().path());
        if let Err(err) = self.save_mem_block() {
            log::error!("Save mem block error {}", err);
        }
        self.restore_manager().delete_before_one_hour();
    }
}

impl MemPool {
    pub async fn create(args: MemPoolCreateArgs) -> Result<Self> {
        let MemPoolCreateArgs {
            block_producer,
            store,
            generator,
            provider,
            config,
            node_mode,
            dynamic_config_manager,
            sync_server,
            account_creator,
        } = args;
        let pending = Default::default();

        let tip: (H256, u64, GlobalState) = {
            let db = &store.begin_transaction();
            let tip_block = db.get_last_valid_tip_block()?;
            let tip_global_state = db
                .get_block_post_global_state(&tip_block.hash())?
                .expect("tip block post global");
            (
                tip_block.hash(),
                tip_block.raw().number().unpack(),
                tip_global_state,
            )
        };
        let tip_hash = tip.0;

        let mut mem_block = MemBlock::with_block_producer(block_producer);
        let mut pending_deposits = vec![];
        let mut pending_restored_tx_hashes = VecDeque::new();

        let restore_manager = RestoreManager::build(&config.restore_path)?;
        if let Ok(Some((restored, timestamp))) = restore_manager.restore_from_latest() {
            log::info!("[mem-pool] restore mem block from timestamp {}", timestamp);

            let hashes: Vec<_> = restored.withdrawals().unpack();
            mem_block.force_reinject_withdrawal_hashes(hashes.as_slice());

            pending_restored_tx_hashes = VecDeque::from(Unpack::<Vec<_>>::unpack(&restored.txs()));
            pending_deposits = restored.deposits().unpack();
            mem_block.append_new_addresses(restored.as_reader().new_addresses().unpack());
        }

        mem_block.clear_txs();

        let mem_pool_state = {
            let state_db = MemStateDB::from_store(store.get_snapshot())?;
            Arc::new(MemPoolState::new(state_db, false))
        };

        let cycles_pool = CyclesPool::new(
            config.mem_block.max_cycles_limit,
            config.mem_block.syscall_cycles.clone(),
        );

        let mut mem_pool = MemPool {
            store,
            current_tip: tip,
            generator,
            pending,
            mem_block,
            provider,
            pending_deposits,
            restore_manager: restore_manager.clone(),
            pending_restored_tx_hashes,
            mem_pool_state,
            dynamic_config_manager,
            sync_server,
            mem_block_config: config.mem_block,
            cycles_pool,
            account_creator,
        };
        mem_pool.restore_pending_withdrawals().await?;
        mem_pool.remove_reinjected_failed_txs()?;

        // update mem block info
        let mut shared = mem_pool.mem_pool_state().load_shared();
        shared.mem_block = Some(mem_pool.mem_block.block_info().to_owned());
        mem_pool.mem_pool_state().store_shared(Arc::new(shared));

        // set tip
        if matches!(node_mode, NodeMode::ReadOnly) {
            mem_pool.reset_read_only(Some(tip_hash), true)?;
        } else {
            mem_pool
                .reset(None, Some(tip_hash), &Default::default())
                .await?;
        }

        // clear stored mem blocks
        tokio::spawn(async move {
            restore_manager.delete_before_one_hour();
        });

        Ok(mem_pool)
    }

    pub fn mem_block(&self) -> &MemBlock {
        &self.mem_block
    }

    pub fn mem_pool_state(&self) -> Arc<MemPoolState> {
        self.mem_pool_state.clone()
    }

    pub fn cycles_pool(&self) -> &CyclesPool {
        &self.cycles_pool
    }

    pub fn cycles_pool_mut(&mut self) -> &mut CyclesPool {
        &mut self.cycles_pool
    }

    pub fn config(&self) -> &MemBlockConfig {
        &self.mem_block_config
    }

    pub fn restore_manager(&self) -> &RestoreManager {
        &self.restore_manager
    }

    pub fn save_mem_block(&mut self) -> Result<()> {
        if !self.pending_restored_tx_hashes.is_empty() {
            log::warn!(
                "save mem block, but have pending restored txs from previous restored mem block"
            );

            self.mem_block.force_reinject_tx_hashes(
                Vec::from_iter(self.pending_restored_tx_hashes.clone()).as_slice(),
            );
        }

        self.restore_manager.save(self.mem_block())
    }

    pub fn save_mem_block_with_suffix(&mut self, suffix: &str) -> Result<()> {
        if !self.pending_restored_tx_hashes.is_empty() {
            log::warn!(
                "save mem block, but have pending restored txs from previous restored mem block"
            );

            self.mem_block.force_reinject_tx_hashes(
                Vec::from(self.pending_restored_tx_hashes.clone()).as_slice(),
            );
        }

        self.restore_manager
            .save_with_suffix(self.mem_block(), suffix)
    }

    pub fn set_provider(&mut self, provider: Box<dyn MemPoolProvider + Send + Sync>) {
        self.provider = provider;
    }

    pub fn is_mem_txs_full(&self, expect_slots: usize) -> bool {
        self.mem_block.txs().len().saturating_add(expect_slots) > self.mem_block_config.max_txs
    }

    pub fn pending_restored_tx_hashes(&mut self) -> &mut VecDeque<H256> {
        &mut self.pending_restored_tx_hashes
    }

    pub fn set_account_creator(&mut self, creator: AccountCreator) {
        self.account_creator = Some(creator);
    }

    /// Push a layer2 tx into pool
    #[instrument(skip_all)]
    pub fn push_transaction(&mut self, tx: L2Transaction) -> Result<()> {
        tokio::task::block_in_place(|| {
            let mut db = self.store.begin_transaction();

            let mut state = self.mem_pool_state.load_state_db();
            self.push_transaction_with_db(&mut db, &mut state, tx)?;
            db.commit()?;
            self.mem_pool_state.store_state_db(state);

            Ok(())
        })
    }

    /// Push a layer2 tx into pool
    #[instrument(skip_all, err(Debug))]
    fn push_transaction_with_db(
        &mut self,
        db: &mut StoreTransaction,
        state: &mut StateDB,
        tx: L2Transaction,
    ) -> Result<()> {
        // check duplication
        let tx_hash: H256 = tx.raw().hash();
        if self.mem_block.txs_set().contains(&tx_hash) {
            return Err(anyhow!("duplicated tx"));
        }

        // reject if mem block is full
        // TODO: we can use the pool as a buffer
        if self.mem_block.txs().len() >= self.mem_block_config.max_txs {
            return Err(anyhow!(
                "Mem block is full, MAX_MEM_BLOCK_TXS: {}",
                self.mem_block_config.max_txs
            ));
        }

        // verify transaction
        let polyjuice_creator_id = self.generator.get_polyjuice_creator_id(state)?;
        TransactionVerifier::new(
            state,
            self.generator.rollup_context(),
            polyjuice_creator_id,
            self.generator.fork_config(),
        )
        .verify(&tx, self.mem_block.block_info().number().unpack())?;
        // verify signature
        self.generator.check_transaction_signature(state, &tx)?;

        // instantly run tx in background & update local state
        let t = Instant::now();
        let tx_receipt = self.execute_tx(db, state, tx.clone())?;
        log::debug!("[push tx] finalize tx time: {}ms", t.elapsed().as_millis());

        // save new addresses
        if self.account_creator.is_some() {
            let logs = tx_receipt.as_reader().logs();
            if let Some(new_addresses) = filter_new_address(logs.iter(), state) {
                self.mem_block.append_new_addresses(new_addresses);
            }
        }

        // save tx receipt in mem pool
        let post_state = tx_receipt.post_state();
        self.mem_block.push_tx(tx_hash, post_state);
        db.insert_mem_pool_transaction_receipt(&tx_hash, tx_receipt)?;

        // Add to pool
        let account_id: u32 = tx.raw().from_id().unpack();
        db.insert_mem_pool_transaction(&tx_hash, tx.clone())?;
        let entry_list = self.pending.entry(account_id).or_default();
        entry_list.txs.push(tx);

        Ok(())
    }

    /// Push a withdrawal request into pool
    #[instrument(skip_all, err(Debug), fields(withdrawal = %withdrawal.hash().pack()))]
    pub async fn push_withdrawal_request(
        &mut self,
        withdrawal: WithdrawalRequestExtra,
    ) -> Result<()> {
        // check duplication
        let withdrawal_hash: H256 = withdrawal.raw().hash();
        if self.mem_block.withdrawals_set().contains(&withdrawal_hash) {
            return Err(anyhow!("duplicated withdrawal"));
        }

        // basic verification without write into state
        // withdrawals will be write into state in the finalize_withdrawals function
        let state = self.mem_pool_state.load_state_db();
        self.verify_withdrawal_request(&withdrawal, &state).await?;

        // Check replace-by-fee
        // TODO

        let account_script_hash: H256 = withdrawal.raw().account_script_hash().unpack();
        let account_id = state
            .get_account_id_by_script_hash(&account_script_hash)?
            .expect("get account_id");
        let entry_list = self.pending.entry(account_id).or_default();
        entry_list.withdrawals.push(withdrawal.clone());
        // Add to pool
        let mut db = self.store.begin_transaction();
        db.insert_mem_pool_withdrawal(&withdrawal_hash, withdrawal)?;
        db.commit()?;
        Ok(())
    }

    // TODO: @sopium optimization: collect on reset and cache.
    fn collect_finalized_custodian_capacity(&self) -> Result<FinalizedCustodianCapacity> {
        if self.current_tip.1 == 0 {
            return Ok(Default::default());
        }
        let snap = self.store.get_snapshot();
        let block: L2Block = snap
            .get_block(&self.current_tip.0)
            .context("get block")?
            .ok_or_else(|| anyhow!("failed to get last block"))?;
        let mut c: FinalizedCustodianCapacity = snap
            .get_block_post_finalized_custodian_capacity(self.current_tip.1)
            .ok_or_else(|| anyhow!("failed to get last block post finalized custodian capacity"))?
            .as_reader()
            .unpack();

        let finalizing_range = calc_finalizing_range(
            &self.generator.rollup_context().rollup_config,
            &self.generator.rollup_context().fork_config,
            &snap,
            &block,
        )?;
        for finalizing_number in finalizing_range {
            let finalizing_deposits = snap
                .get_block_deposit_info_vec(finalizing_number)
                .context("get last finalized block deposit")?;
            for i in finalizing_deposits {
                let d = i.request();
                c.capacity += u128::from(d.capacity().unpack());
                let amount = d.amount().unpack();
                if amount > 0 {
                    let hash = d.sudt_script_hash().unpack();
                    c.checked_add_sudt(hash, amount, d.script())
                        .expect("add sudt amount overflow");
                }
            }
        }
        Ok(c)
    }
    // Withdrawal request verification
    // TODO: duplicate withdrawal check
    #[instrument(skip_all)]
    async fn verify_withdrawal_request(
        &self,
        withdrawal: &WithdrawalRequestExtra,
        state: &(impl State + CodeStore),
    ) -> Result<()> {
        // verify withdrawal signature
        self.generator
            .check_withdrawal_signature(state, withdrawal)?;

        let finalized_custodian_capacity = self.collect_finalized_custodian_capacity()?;
        let withdrawal_generator = WithdrawalGenerator::new(
            self.generator.rollup_context(),
            finalized_custodian_capacity,
        );
        withdrawal_generator.verify_remained_amount(&withdrawal.request())?;

        // withdrawal basic verification
        let db = &self.store.begin_transaction();
        let asset_script = db.get_asset_script(&withdrawal.raw().sudt_script_hash().unpack())?;
        WithdrawalVerifier::new(
            state,
            self.generator.rollup_context(),
            self.generator.fork_config(),
        )
        .verify(
            withdrawal,
            asset_script,
            self.mem_block.block_info().number().unpack(),
        )
        .map_err(Into::into)
    }

    /// Return pending contents
    fn pending(&self) -> &HashMap<u32, EntryList> {
        &self.pending
    }

    /// Notify new tip
    /// this method update current state of mem pool
    ///
    /// This method should only be used on a full node or test node.
    #[instrument(skip_all)]
    pub async fn notify_new_tip(
        &mut self,
        new_tip: H256,
        local_cells_manager: &LocalCellsManager,
    ) -> Result<()> {
        // reset pool state
        if self.current_tip.0 != new_tip {
            self.reset(Some(self.current_tip.0), Some(new_tip), local_cells_manager)
                .await?;
        }
        Ok(())
    }

    /// Clear mem block state and recollect deposits
    #[instrument(skip_all)]
    pub async fn reset_mem_block(&mut self, local_cells_manager: &LocalCellsManager) -> Result<()> {
        log::info!("[mem-pool] reset mem block");
        // reset pool state
        self.reset(
            Some(self.current_tip.0),
            Some(self.current_tip.0),
            local_cells_manager,
        )
        .await?;
        Ok(())
    }

    /// output mem block
    #[instrument(skip_all, fields(retry_count = output_param.retry_count))]
    pub fn output_mem_block(&self, output_param: &OutputParam) -> (MemBlock, AccountMerkleState) {
        Self::package_mem_block(&self.mem_block, output_param)
    }

    pub(crate) fn package_mem_block(
        mem_block: &MemBlock,
        output_param: &OutputParam,
    ) -> (MemBlock, AccountMerkleState) {
        let (withdrawals_count, deposits_count, txs_count) =
            repackage_count(mem_block, output_param);

        log::info!(
            "[mem-pool] package mem block, retry count {}",
            output_param.retry_count
        );
        mem_block.repackage(withdrawals_count, deposits_count, txs_count)
    }

    /// Reset pool
    ///
    /// This method reset the current state of the mem pool. Discarded txs &
    /// withdrawals will be reinject to pool.
    ///
    /// This method should only be used on a full node or test node.
    #[instrument(skip_all, fields(old_tip = old_tip.map(|h| display(h.pack())), new_tip = new_tip.map(|h| display(h.pack()))))]
    async fn reset(
        &mut self,
        old_tip: Option<H256>,
        new_tip: Option<H256>,
        local_cells_manager: &LocalCellsManager,
    ) -> Result<()> {
        self.reset_full(old_tip, new_tip, local_cells_manager).await
    }

    /// Only **ReadOnly** node.
    /// update current tip. Reset mem pool state if `update_state` is true.
    #[instrument(skip_all)]
    pub fn reset_read_only(&mut self, new_tip: Option<H256>, update_state: bool) -> Result<()> {
        let new_tip = match new_tip {
            Some(block_hash) => block_hash,
            None => {
                log::debug!("reset new tip to last valid tip block");
                self.store.get_last_valid_tip_block_hash()?
            }
        };
        let new_tip_block = self.store.get_block(&new_tip)?.expect("new tip block");
        let new_tip_global_state = self
            .store
            .get_block_post_global_state(&new_tip)?
            .expect("new tip global state");
        self.current_tip = (
            new_tip,
            new_tip_block.raw().number().unpack(),
            new_tip_global_state,
        );
        if update_state {
            // For read only nodes that does not have P2P mem-pool syncing, just
            // reset mem block and mem pool state. Mem block will be mostly
            // empty and not in sync with full node anyway, so we skip
            // re-injecting discarded txs/withdrawals.
            let snapshot = self.store.get_snapshot();
            self.mem_block.reset(&new_tip_block, Duration::ZERO);
            let shared = mem_pool_state::Shared {
                state_db: MemStateDB::from_store(snapshot)?,
                mem_block: Some(self.mem_block.block_info().to_owned()),
            };
            self.mem_pool_state.store_shared(Arc::new(shared));
        }

        Ok(())
    }

    /// Only **Full** node and **Test** node.
    /// reset mem pool state
    #[instrument(skip_all)]
    async fn reset_full(
        &mut self,
        old_tip: Option<H256>,
        new_tip: Option<H256>,
        local_cells_manager: &LocalCellsManager,
    ) -> Result<()> {
        let mut reinject_txs = Default::default();
        let mut reinject_withdrawals = Default::default();
        // read block from db
        let new_tip = match new_tip {
            Some(block_hash) => block_hash,
            None => {
                log::debug!("reset new tip to last valid tip block");
                self.store.get_last_valid_tip_block_hash()?
            }
        };
        let new_tip_block = self.store.get_block(&new_tip)?.expect("new tip block");

        if old_tip.is_some() && old_tip != Some(new_tip_block.raw().parent_block_hash().unpack()) {
            let old_tip = old_tip.unwrap();
            let old_tip_block = self.store.get_block(&old_tip)?.expect("old tip block");

            let new_number: u64 = new_tip_block.raw().number().unpack();
            let old_number: u64 = old_tip_block.raw().number().unpack();
            let depth = max(new_number, old_number) - min(new_number, old_number);
            if depth > 64 {
                log::error!("skipping deep transaction reorg: depth {}", depth);
            } else {
                let mut rem = old_tip_block;
                let mut add = new_tip_block.clone();
                let mut discarded_txs: VecDeque<L2Transaction> = Default::default();
                let mut included_txs: HashSet<L2Transaction> = Default::default();
                let mut discarded_withdrawals: VecDeque<WithdrawalRequestExtra> =
                    Default::default();
                let mut included_withdrawals: HashSet<WithdrawalRequest> = Default::default();
                while rem.raw().number().unpack() > add.raw().number().unpack() {
                    // reverse push, so we can keep txs in block's order
                    for index in (0..rem.transactions().len()).rev() {
                        discarded_txs.push_front(rem.transactions().get(index).unwrap());
                    }
                    let block_hash = rem.hash();
                    // reverse push, so we can keep withdrawals in block's order
                    for index in (0..rem.withdrawals().len()).rev() {
                        let key =
                            WithdrawalKey::build_withdrawal_key(block_hash.pack(), index as u32);
                        let withdrawal = rem.withdrawals().get(index).unwrap();
                        let withdrawal_extra = self
                            .store
                            .get_withdrawal_by_key(&key)?
                            .expect("get withdrawal");
                        assert_eq!(withdrawal, withdrawal_extra.request());
                        discarded_withdrawals.push_front(withdrawal_extra);
                    }
                    rem = self
                        .store
                        .get_block(&rem.raw().parent_block_hash().unpack())?
                        .expect("get block");
                }
                while add.raw().number().unpack() > rem.raw().number().unpack() {
                    included_txs.extend(add.transactions().into_iter());
                    included_withdrawals.extend(rem.withdrawals().into_iter());
                    add = self
                        .store
                        .get_block(&add.raw().parent_block_hash().unpack())?
                        .expect("get block");
                }
                while rem.hash() != add.hash() {
                    // reverse push, so we can keep txs in block's order
                    for index in (0..rem.transactions().len()).rev() {
                        discarded_txs.push_front(rem.transactions().get(index).unwrap());
                    }
                    // reverse push, so we can keep withdrawals in block's order
                    for index in (0..rem.withdrawals().len()).rev() {
                        let withdrawal = rem.withdrawals().get(index).unwrap();
                        let withdrawal_extra = self
                            .store
                            .get_withdrawal(&withdrawal.hash())?
                            .expect("get withdrawal");
                        discarded_withdrawals.push_front(withdrawal_extra);
                    }
                    rem = self
                        .store
                        .get_block(&rem.raw().parent_block_hash().unpack())?
                        .expect("get block");
                    included_txs.extend(add.transactions().into_iter());
                    included_withdrawals.extend(add.withdrawals().into_iter());
                    add = self
                        .store
                        .get_block(&add.raw().parent_block_hash().unpack())?
                        .expect("get block");
                }
                // remove included txs
                discarded_txs.retain(|tx| !included_txs.contains(tx));
                reinject_txs = discarded_txs;
                // remove included withdrawals
                discarded_withdrawals
                    .retain(|withdrawal| !included_withdrawals.contains(&withdrawal.request()));
                reinject_withdrawals = discarded_withdrawals
                    .into_iter()
                    .map(Into::<WithdrawalRequestExtra>::into)
                    .collect::<VecDeque<_>>()
            }
        }

        let mut db = self.store.begin_transaction();

        let is_mem_pool_recovery = old_tip.is_none();

        // refresh pending deposits
        if !is_mem_pool_recovery {
            self.refresh_deposit_cells(&db, new_tip, local_cells_manager)
                .await?;
        }

        // estimate next l2block timestamp
        let estimated_timestamp = {
            let estimated = self.provider.estimate_next_blocktime().await;
            let tip_timestamp = Duration::from_millis(new_tip_block.raw().timestamp().unpack());
            match estimated {
                Ok(e) if e <= tip_timestamp => tip_timestamp.saturating_add(Duration::from_secs(1)),
                Err(_) => tip_timestamp.saturating_add(Duration::from_secs(1)),
                Ok(e) => e,
            }
        };

        block_in_place(move || {
            // reset mem block state
            let snapshot = self.store.get_snapshot();
            let snap_last_valid_tip = snapshot.get_last_valid_tip_block_hash()?;
            assert_eq!(snap_last_valid_tip, new_tip, "set new snapshot");

            let mem_block_content = self.mem_block.reset(&new_tip_block, estimated_timestamp);

            // set tip
            let new_tip_global_state = self
                .store
                .get_block_post_global_state(&new_tip)?
                .expect("new tip block global state");
            self.current_tip = (
                new_tip,
                new_tip_block.raw().number().unpack(),
                new_tip_global_state,
            );

            // mem block withdrawals
            let mem_block_withdrawals: Vec<_> = {
                let mut withdrawals = Vec::with_capacity(mem_block_content.withdrawals.len());
                for withdrawal_hash in mem_block_content.withdrawals {
                    if let Some(withdrawal) = db.get_mem_pool_withdrawal(&withdrawal_hash)? {
                        withdrawals.push(withdrawal);
                    }
                }
                withdrawals
            };

            // mem block txs
            let mem_block_txs: Vec<_> = {
                let mut txs = Vec::with_capacity(mem_block_content.txs.len());
                for tx_hash in mem_block_content.txs {
                    if let Some(tx) = db.get_mem_pool_transaction(&tx_hash)? {
                        txs.push(tx);
                    }
                }
                txs
            };

            // create new mem_store to maintain memory state
            let mut state_db = StateDB::from_store(snapshot)?;
            let mem_block = self.mem_block.block_info().to_owned();

            // remove from pending
            self.remove_unexecutables(&mut state_db, &mut db)?;

            log::info!("[mem-pool] reset reinject txs: {} mem-block txs: {} reinject withdrawals: {} mem-block withdrawals: {}", reinject_txs.len(), mem_block_txs.len(), reinject_withdrawals.len(), mem_block_withdrawals.len());
            // re-inject txs
            let txs = reinject_txs.into_iter().chain(mem_block_txs).collect();

            // re-inject withdrawals
            let mut withdrawals: Vec<_> = reinject_withdrawals.into_iter().collect();
            if is_mem_pool_recovery {
                // recovery mem block withdrawals
                withdrawals.extend(mem_block_withdrawals);
            } else {
                // packages more withdrawals
                self.try_package_more_withdrawals(&state_db, &mut withdrawals);
            }

            // To simplify logic, don't restrict reinjected txs
            self.cycles_pool = CyclesPool::new(u64::MAX, SyscallCyclesConfig::default());

            self.prepare_next_mem_block(
                &mut db,
                &mut state_db,
                withdrawals,
                self.pending_deposits.clone(),
                txs,
            )?;

            // create account for new addresses
            if let Some(account_creator) = self.account_creator.as_ref() {
                match account_creator
                    .build_batch_create_tx(&state_db, mem_block_content.new_addresses)
                {
                    Ok(Some((tx, next_batch))) => {
                        self.mem_block.append_new_addresses(next_batch);
                        if let Err(err) = self.push_transaction_with_db(&mut db, &mut state_db, tx)
                        {
                            tracing::error!("account creator err {}", err);
                        }
                    }
                    Err(err) => {
                        tracing::error!("account creator err {}", err);
                    }
                    Ok(None) => (),
                }
            }

            // Update block remained cycles
            let used_cycles = self.cycles_pool.cycles_used();
            self.cycles_pool = CyclesPool::new(
                self.mem_block_config.max_cycles_limit,
                self.mem_block_config.syscall_cycles.clone(),
            );
            self.cycles_pool.consume_cycles(used_cycles);

            // store mem state
            let shared = Shared {
                state_db,
                mem_block: Some(mem_block),
            };
            self.mem_pool_state.store_shared(Arc::new(shared));
            db.commit()?;

            Ok(())
        })
    }

    fn try_package_more_withdrawals(
        &self,
        state: &StateDB,
        withdrawals: &mut Vec<WithdrawalRequestExtra>,
    ) {
        // packages mem withdrawals
        fn filter_withdrawals(state: &StateDB, withdrawal: &WithdrawalRequestExtra) -> bool {
            let id = state
                .get_account_id_by_script_hash(&withdrawal.raw().account_script_hash().unpack())
                .expect("get id")
                .expect("id exist");
            let nonce = state.get_nonce(id).expect("get nonce");
            let expected_nonce: u32 = withdrawal.raw().nonce().unpack();
            expected_nonce >= nonce
        }
        withdrawals.retain(|w| filter_withdrawals(state, w));

        // package withdrawals
        if withdrawals.len() < self.mem_block_config.max_withdrawals {
            for entry in self.pending().values() {
                if let Some(withdrawal) = entry.withdrawals.first() {
                    if filter_withdrawals(state, withdrawal) {
                        withdrawals.push(withdrawal.clone());
                    }
                    if withdrawals.len() >= self.mem_block_config.max_withdrawals {
                        break;
                    }
                }
            }
        }
    }

    /// Discard unexecutables from pending.
    #[instrument(skip_all)]
    fn remove_unexecutables(
        &mut self,
        state: &mut StateDB,
        db: &mut StoreTransaction,
    ) -> Result<()> {
        let mut remove_list = Vec::default();
        // iter pending accounts and demote any non-executable objects
        for (&account_id, list) in &mut self.pending {
            let nonce = state.get_nonce(account_id)?;

            // drop txs if tx.nonce lower than nonce
            let deprecated_txs = list.remove_lower_nonce_txs(nonce);
            for tx in deprecated_txs {
                let tx_hash = tx.hash();
                db.remove_mem_pool_transaction(&tx_hash)?;
            }
            // Drop all withdrawals that are have no enough balance
            let script_hash = state.get_script_hash(account_id)?;
            if let Some(registry_id) = list
                .withdrawals
                .first()
                .map(|first| first.request().raw().registry_id().unpack())
            {
                let address = state
                    .get_registry_address_by_script_hash(registry_id, &script_hash)?
                    .expect("must exist");
                let capacity = CKBCapacity::from_layer2(
                    state.get_sudt_balance(CKB_SUDT_ACCOUNT_ID, &address)?,
                );
                let deprecated_withdrawals = list.remove_lower_nonce_withdrawals(nonce, capacity);
                for withdrawal in deprecated_withdrawals {
                    let withdrawal_hash: H256 = withdrawal.hash();
                    db.remove_mem_pool_withdrawal(&withdrawal_hash)?;
                }
            }
            // Delete empty entry
            if list.is_empty() {
                remove_list.push(account_id);
            }
        }
        for account_id in remove_list {
            self.pending.remove(&account_id);
        }
        Ok(())
    }

    /// Prepare for next mem block
    #[instrument(skip_all, fields(withdrawals_count = withdrawals.len(), txs_count = txs.len()))]
    fn prepare_next_mem_block(
        &mut self,
        db: &mut StoreTransaction,
        state: &mut StateDB,
        withdrawals: Vec<WithdrawalRequestExtra>,
        deposit_cells: Vec<DepositInfo>,
        mut txs: Vec<L2Transaction>,
    ) -> Result<()> {
        // remove txs nonce is lower than current state
        fn filter_tx(state: &StateDB, tx: &L2Transaction) -> bool {
            let raw_tx = tx.raw();
            let nonce = state
                .get_nonce(raw_tx.from_id().unpack())
                .expect("get nonce");
            let expected_nonce: u32 = raw_tx.nonce().unpack();
            expected_nonce >= nonce
        }
        txs.retain(|tx| filter_tx(state, tx));
        // check order of inputs
        {
            let mut id_to_nonce: HashMap<u32, u32> = HashMap::default();
            for tx in &txs {
                let id: u32 = tx.raw().from_id().unpack();
                let nonce: u32 = tx.raw().nonce().unpack();
                if let Some(&prev_nonce) = id_to_nonce.get(&id) {
                    assert!(
                        nonce > prev_nonce,
                        "id: {} nonce({}) > prev_nonce({})",
                        id,
                        nonce,
                        prev_nonce
                    );
                }
                id_to_nonce.entry(id).or_insert(nonce);
            }
        }
        // Handle state before txs
        // withdrawal
        self.finalize_withdrawals(state, db, withdrawals.clone())?;
        // deposits
        self.finalize_deposits(state, deposit_cells.clone())?;

        if let Some(ref sync_server) = self.sync_server {
            let mut sync_server = sync_server.lock().unwrap();
            sync_server.publish_next_mem_block(
                NextMemBlock::new_builder()
                    .block_info(self.mem_block.block_info().clone())
                    .withdrawals(withdrawals.pack())
                    .deposits(deposit_cells.pack())
                    .build(),
            );
        }

        // re-inject txs
        for tx in txs {
            if let Err(err) = self.push_transaction_with_db(db, state, tx.clone()) {
                let tx_hash = tx.hash();
                log::info!(
                    "[mem pool] fail to re-inject tx {}, error: {}",
                    hex::encode(&tx_hash),
                    err
                );
            }
        }

        Ok(())
    }

    /// refresh pending deposits
    #[instrument(skip_all)]
    async fn refresh_deposit_cells(
        &mut self,
        db: &StoreTransaction,
        new_block_hash: H256,
        local_cells_manager: &LocalCellsManager,
    ) -> Result<()> {
        // refresh
        let state = self.mem_pool_state.load_state_db();
        let mem_account_count = state.get_account_count()?;
        let tip_account_count: u32 = {
            let new_tip_block = db
                .get_block(&new_block_hash)?
                .ok_or_else(|| anyhow!("can't find new tip block"))?;
            new_tip_block.raw().post_account().count().unpack()
        };

        log::debug!(
            "[mem-pool] refresh pending deposits, mem_account_count: {}, tip_account_count: {}",
            mem_account_count,
            tip_account_count
        );
        let cells = self
            .provider
            .collect_deposit_cells(local_cells_manager)
            .await?;
        self.pending_deposits = crate::deposit::sanitize_deposit_cells(
            self.generator.rollup_context(),
            &self.mem_block_config.deposit_timeout_config,
            cells,
            &state,
        );
        log::debug!(
            "[mem-pool] refreshed deposits: {}",
            self.pending_deposits.len()
        );

        Ok(())
    }

    #[instrument(skip_all, fields(deposits_count = deposit_cells.len()))]
    fn finalize_deposits(
        &mut self,
        state: &mut StateDB,
        deposit_cells: Vec<DepositInfo>,
    ) -> Result<()> {
        state.set_state_tracker(Default::default());
        // update deposits
        let deposits: Vec<_> = deposit_cells.iter().map(|c| c.request.clone()).collect();
        let mut post_states = Vec::with_capacity(deposits.len());
        let mut touched_keys_vec = Vec::with_capacity(deposits.len());
        for deposit in deposits {
            state.apply_deposit_request(self.generator.rollup_context(), &deposit)?;
            let touched_keys = state.state_tracker().unwrap().touched_keys();
            touched_keys_vec.push(touched_keys.lock().unwrap().drain().collect());
            state.finalise()?;
            post_states.push(state.calculate_merkle_state()?);
        }
        state.take_state_tracker();
        // calculate state after withdrawals & deposits
        let prev_state_checkpoint = state.calculate_state_checkpoint()?;
        log::debug!("[finalize deposits] deposits: {} state root: {}, account count: {}, prev_state_checkpoint {}",
         deposit_cells.len(), hex::encode(state.calculate_root()?.as_slice()), state.get_account_count()?, hex::encode(prev_state_checkpoint.as_slice()));

        self.mem_block.push_deposits(
            deposit_cells,
            post_states,
            touched_keys_vec,
            prev_state_checkpoint,
        );

        Ok(())
    }

    /// Execute withdrawal & update local state
    #[instrument(skip_all, fields(withdrawals_count = withdrawals.len()))]
    fn finalize_withdrawals(
        &mut self,
        state: &mut StateDB,
        db: &mut StoreTransaction,
        withdrawals: Vec<WithdrawalRequestExtra>,
    ) -> Result<()> {
        // check mem block state
        assert!(self.mem_block.withdrawals().is_empty());
        assert!(self.mem_block.state_checkpoints().is_empty());
        assert!(self.mem_block.deposits().is_empty());
        assert!(self.mem_block.finalized_custodians().is_empty());
        assert!(self.mem_block.txs().is_empty());

        let finalized_custodians = self.collect_finalized_custodian_capacity()?;
        let asset_scripts: HashMap<H256, Script> = {
            let sudt_value = finalized_custodians.sudt.values();
            sudt_value.map(|(_, script)| (script.hash(), script.to_owned()))
        }
        .collect();
        // verify the withdrawals
        let mut unused_withdrawals = Vec::with_capacity(withdrawals.len());
        let mut withdrawal_verifier = crate::withdrawal::Generator::new(
            self.generator.rollup_context(),
            finalized_custodians,
        );
        // start track withdrawal
        state.set_state_tracker(Default::default());
        for withdrawal in withdrawals {
            let withdrawal_hash = withdrawal.hash();
            // check withdrawal request
            if let Err(err) = self
                .generator
                .check_withdrawal_signature(state, &withdrawal)
            {
                log::info!("[mem-pool] withdrawal signature error: {:?}", err);
                unused_withdrawals.push(withdrawal_hash);
                continue;
            }
            let asset_script = {
                let script_hash: H256 = withdrawal.raw().sudt_script_hash().unpack();
                asset_scripts.get(&script_hash).cloned()
            };
            if let Err(err) = WithdrawalVerifier::new(
                state,
                self.generator.rollup_context(),
                self.generator.fork_config(),
            )
            .verify(
                &withdrawal,
                asset_script,
                self.mem_block.block_info().number().unpack(),
            ) {
                log::info!("[mem-pool] withdrawal verification error: {:?}", err);
                unused_withdrawals.push(withdrawal_hash);
                continue;
            }

            if let Err(err) =
                withdrawal_verifier.include_and_verify(&withdrawal, &L2Block::default())
            {
                log::info!(
                    "[mem-pool] withdrawal contextual verification failed : {}",
                    err
                );
                unused_withdrawals.push(withdrawal_hash);
                continue;
            }

            // update the state
            let snap = state.snapshot();
            match state.apply_withdrawal_request(
                self.generator.rollup_context(),
                self.mem_block.block_producer(),
                &withdrawal.request(),
            ) {
                Ok(_) => {
                    state.finalise()?;
                    let post_state = state.calculate_merkle_state()?;
                    let touched_keys = state.state_tracker().unwrap().touched_keys();

                    let withdrawal_hash = withdrawal.hash();

                    // Add to pending list and db if the withdrawal isn't
                    // already in them. This can happen when the withdrawal is
                    // re-injected because the L2 block packaging it is
                    // reverted.
                    let account_script_hash: H256 = withdrawal.raw().account_script_hash().unpack();
                    let account_id = state
                        .get_account_id_by_script_hash(&account_script_hash)?
                        .expect("get account_id");
                    let entry_list = self.pending.entry(account_id).or_default();
                    if !entry_list.withdrawals.contains(&withdrawal) {
                        entry_list.withdrawals.push(withdrawal.clone());
                        db.insert_mem_pool_withdrawal(&withdrawal_hash, withdrawal)?;
                    }

                    self.mem_block.push_withdrawal(
                        withdrawal_hash,
                        post_state,
                        touched_keys.lock().unwrap().drain(),
                    );
                }
                Err(err) => {
                    log::info!("[mem-pool] withdrawal execution failed : {}", err);
                    state.revert(snap)?;
                    unused_withdrawals.push(withdrawal_hash);
                }
            }
        }
        state.take_state_tracker();
        self.mem_block
            .set_finalized_custodian_capacity(withdrawal_verifier.remaining_capacity());

        // remove unused withdrawals
        log::info!(
            "[mem-pool] finalize withdrawals: {} staled withdrawals: {}",
            self.mem_block.withdrawals().len(),
            unused_withdrawals.len()
        );
        Ok(())
    }

    /// Execute tx & update local state
    #[instrument(skip_all)]
    fn execute_tx(
        &mut self,
        db: &StoreTransaction,
        state: &mut StateDB,
        tx: L2Transaction,
    ) -> Result<TxReceipt> {
        let tip_block_hash = db.get_tip_block_hash()?;
        let chain_view = ChainView::new(&db, tip_block_hash);

        let block_info = self.mem_block.block_info();

        // check allow list
        if let Some(polyjuice_contract_creator_allowlist) = self
            .dynamic_config_manager
            .load()
            .get_polyjuice_contract_creator_allowlist()
        {
            use gw_tx_filter::polyjuice_contract_creator_allowlist::Error;

            match polyjuice_contract_creator_allowlist.validate_with_state(state, &tx.raw()) {
                Ok(_) => (),
                Err(Error::Common(err)) => return Err(TransactionError::from(err).into()),
                Err(Error::ScriptHashNotFound) => {
                    return Result::<_>::Err(TransactionError::ScriptHashNotFound.into())
                        .context("failed to check contract creator allowlist")
                }
                Err(Error::PermissionDenied { account_id }) => {
                    return Err(TransactionError::InvalidContractCreatorAccount {
                        backend: "polyjuice",
                        account_id,
                    }
                    .into())
                }
            }
        }

        let cycles_pool = &mut self.cycles_pool;
        let generator = Arc::clone(&self.generator);

        // execute tx
        let raw_tx = tx.raw();
        let snap = state.snapshot();
        let run_result = generator
            .execute_transaction(
                &chain_view,
                state,
                block_info,
                &raw_tx,
                None,
                Some(cycles_pool),
            )
            .map_err(|err| {
                // revert state
                state.revert(snap).unwrap();
                err
            })?;

        // check account id of sudt proxy contract creator is from whitelist
        {
            let from_id = raw_tx.from_id().unpack();
            if !self
                .dynamic_config_manager
                .load()
                .get_sudt_proxy_account_whitelist()
                .validate(&run_result, from_id)
            {
                // revert state
                state.revert(snap)?;
                return Err(TransactionError::InvalidSUDTProxyCreatorAccount {
                    account_id: from_id,
                }
                .into());
            }
        }

        state.finalise()?;
        // finalise dirty state
        let merkle_state = state.calculate_merkle_state()?;

        // generate tx receipt
        let tx_receipt = TxReceipt::build_receipt(tx.witness_hash(), run_result, merkle_state);

        if let Some(ref sync_server) = self.sync_server {
            sync_server.lock().unwrap().publish_transaction(tx);
        }

        Ok(tx_receipt)
    }

    async fn restore_pending_withdrawals(&mut self) -> Result<()> {
        let db = self.store.begin_transaction();
        let mut db1 = self.store.begin_transaction();
        let withdrawals_iter = db.get_mem_pool_withdrawal_iter();

        for (withdrawal_hash, withdrawal) in withdrawals_iter {
            if self.mem_block.withdrawals_set().contains(&withdrawal_hash) {
                continue;
            }

            if let Err(err) = self.push_withdrawal_request(withdrawal).await {
                // Outdated withdrawal in db before bug fix
                log::info!(
                    "[mem-pool] withdrawal restore outdated pending {:x} {}, drop it",
                    withdrawal_hash.pack(),
                    err
                );
                db1.remove_mem_pool_withdrawal(&withdrawal_hash)?;
            }
        }

        db1.commit()?;
        Ok(())
    }

    // Remove re-injected failed txs in mem pool db before bug fix.
    // These txs depend on auto create tx to create sender accounts. Because we package
    // new deposits during mem block reset, make these txs' from id invalid and re-injected failed.
    fn remove_reinjected_failed_txs(&mut self) -> Result<()> {
        let db = self.store.begin_transaction();
        let mut db1 = self.store.begin_transaction();
        let txs_iter = db.get_mem_pool_transaction_iter();

        for (tx_hash, _) in txs_iter {
            if self.mem_block.txs_set().contains(&tx_hash)
                || self.pending_restored_tx_hashes.contains(&tx_hash)
            {
                continue;
            }

            log::info!(
                "[mem-pool] remove re-injected failed tx {:x}",
                tx_hash.pack()
            );
            db1.remove_mem_pool_transaction(&tx_hash)?;
        }

        db1.commit()?;
        Ok(())
    }

    // Only **ReadOnly** node needs this.
    // Refresh mem block with those params.
    // Always expects next block number equals with current_tip_block_number + 1.
    // This function returns Ok(Some(block_number)), if refresh is successful.
    // Or returns Ok(None) if current tip has not synced yet.
    #[instrument(skip_all, fields(block = block_info.number().unpack(), withdrawals_count = withdrawals.len(), deposits_count = deposits.len()))]
    pub fn refresh_mem_block(
        &mut self,
        block_info: BlockInfo,
        mut withdrawals: Vec<WithdrawalRequestExtra>,
        deposits: Vec<DepositInfo>,
    ) -> Result<Option<u64>> {
        block_in_place(move || {
            let next_block_number = block_info.number().unpack();
            let current_tip_block_number = self.current_tip.1;
            if next_block_number <= current_tip_block_number {
                // mem blocks from the past should be ignored
                log::trace!(
                    "Ignore this mem block: {}, current tip: {}",
                    next_block_number,
                    current_tip_block_number
                );
                return Ok(Some(current_tip_block_number));
            }
            if next_block_number != current_tip_block_number + 1 {
                return Ok(None);
            }
            let snapshot = self.store.get_snapshot();
            let tip_block = snapshot.get_last_valid_tip_block()?;

            // mem block txs
            let mem_block_txs: Vec<_> = {
                let mut txs = Vec::with_capacity(self.mem_block.txs().len());
                for tx_hash in self.mem_block.txs() {
                    if let Some(tx) = snapshot.get_mem_pool_transaction(tx_hash)? {
                        txs.push(tx);
                    }
                }
                txs
            };

            // update mem block
            let post_merkle_state = tip_block.raw().post_account();
            let enforce_correctness_of_state_checkpoint_list = self
                .generator
                .fork_config()
                .enforce_correctness_of_state_checkpoint_list(next_block_number);
            let mem_block = MemBlock::new(
                block_info,
                post_merkle_state,
                enforce_correctness_of_state_checkpoint_list,
            );
            self.mem_block = mem_block;

            let mut state = StateDB::from_store(snapshot)?;
            let mem_block = self.mem_block.block_info().to_owned();

            // remove from pending
            let mut db = self.store.begin_transaction();
            self.remove_unexecutables(&mut state, &mut db)?;

            // reset cycles pool available cycles.
            self.cycles_pool = CyclesPool::new(u64::MAX, SyscallCyclesConfig::default());

            // prepare next mem block
            self.try_package_more_withdrawals(&state, &mut withdrawals);
            self.prepare_next_mem_block(&mut db, &mut state, withdrawals, deposits, mem_block_txs)?;

            // update mem state
            let shared = Shared {
                state_db: state,
                mem_block: Some(mem_block),
            };
            self.mem_pool_state.store_shared(Arc::new(shared));
            db.commit()?;

            let mem_block = &self.mem_block;
            log::info!(
                "Refreshed mem_block: block id: {}, deposits: {}, withdrawals: {}, txs: {}",
                mem_block.block_info().number().unpack(),
                mem_block.deposits().len(),
                mem_block.withdrawals().len(),
                mem_block.txs().len()
            );

            Ok(Some(next_block_number))
        })
    }
}

pub(crate) fn repackage_count(
    mem_block: &MemBlock,
    output_param: &OutputParam,
) -> (usize, usize, usize) {
    let total = mem_block.withdrawals().len() + mem_block.deposits().len() + mem_block.txs().len();
    // Drop base on retry count
    let mut remain = total.shr(output_param.retry_count);
    if 0 == remain {
        // Package at least one
        remain = 1;
    }

    let withdrawals_count = mem_block.withdrawals().iter().take(remain).count();
    remain = remain.saturating_sub(withdrawals_count);

    let deposits_count = mem_block.deposits().iter().take(remain).count();
    remain = remain.saturating_sub(deposits_count);

    let txs_count = mem_block.txs().iter().take(remain).count();

    (withdrawals_count, deposits_count, txs_count)
}

#[cfg(test)]
mod test {
    use std::ops::Shr;

    use gw_common::merkle_utils::calculate_state_checkpoint;
    use gw_common::registry_address::RegistryAddress;
    use gw_types::h256::*;
    use gw_types::offchain::{DepositInfo, FinalizedCustodianCapacity};
    use gw_types::packed::{AccountMerkleState, BlockInfo, DepositRequest};
    use gw_types::prelude::{Builder, Entity, Pack, Unpack};

    use crate::mem_block::{MemBlock, MemBlockCmp};
    use crate::pool::{repackage_count, MemPool, OutputParam};

    #[test]
    fn test_package_mem_block() {
        let block_info = {
            let address = RegistryAddress::default();
            BlockInfo::new_builder()
                .block_producer(address.to_bytes().pack())
                .build()
        };
        let prev_merkle_state = AccountMerkleState::new_builder().count(3u32.pack()).build();

        // Random withdrawals
        let withdrawals_count = 50;
        let withdrawals: Vec<_> = (0..withdrawals_count).map(|_| random_hash()).collect();
        let withdrawals_touch_keys: Vec<_> =
            { (0..withdrawals_count).map(|_| vec![random_hash()]) }.collect();
        let withdrawals_state: Vec<_> = (0..withdrawals_count).map(|_| random_state()).collect();

        // Random deposits
        let deposits_count = 50;
        let deposits: Vec<_> = {
            (0..deposits_count).map(|_| DepositInfo {
                request: DepositRequest::new_builder()
                    .sudt_script_hash(random_hash().pack())
                    .build(),
                ..Default::default()
            })
        }
        .collect();
        let deposits_touch_keys: Vec<_> =
            (0..deposits_count).map(|_| vec![random_hash()]).collect();
        let deposits_state: Vec<_> = { (0..deposits_count).map(|_| random_state()) }.collect();

        let finalized_custodians = FinalizedCustodianCapacity::default();

        // Random txs
        let txs_count = 500;
        let txs: Vec<_> = (0..txs_count).map(|_| random_hash()).collect();
        let txs_state: Vec<_> = (0..txs_count).map(|_| random_state()).collect();

        // Fill mem block
        let mem_block = {
            let mut mem_block = MemBlock::new(block_info.clone(), prev_merkle_state.clone(), true);
            for ((hash, touched_keys), state) in { withdrawals.clone().into_iter() }
                .zip(withdrawals_touch_keys.clone())
                .zip(withdrawals_state.clone())
            {
                mem_block.push_withdrawal(hash, state, touched_keys.into_iter());
            }
            mem_block.set_finalized_custodian_capacity(finalized_custodians.clone());

            let txs_prev_state_checkpoint = {
                let state = deposits_state.last().unwrap();
                calculate_state_checkpoint(&state.merkle_root().unpack(), state.count().unpack())
            };
            mem_block.push_deposits(
                deposits.clone(),
                deposits_state.clone(),
                deposits_touch_keys.clone(),
                txs_prev_state_checkpoint,
            );
            for (hash, state) in txs.clone().into_iter().zip(txs_state.clone()) {
                mem_block.push_tx(hash, state);
            }

            mem_block
        };

        // Retry count 0, package whole mem block
        let (mem_block_out, post_block_state) =
            MemPool::package_mem_block(&mem_block, &OutputParam { retry_count: 0 });
        let expected_block = &mem_block;

        // Check output mem block
        assert_eq!(mem_block_out.cmp(expected_block), MemBlockCmp::Same);
        assert_eq!(
            &post_block_state,
            mem_block.tx_post_states().last().unwrap()
        );

        let repackage = |withdrawals_count, deposits_count, txs_count| -> _ {
            let mut expected = MemBlock::new(block_info.clone(), prev_merkle_state.clone(), true);
            let mut post_states = vec![prev_merkle_state.clone()];
            for ((hash, touched_keys), state) in { withdrawals.clone().into_iter() }
                .zip(withdrawals_touch_keys.clone())
                .zip(withdrawals_state.clone())
                .take(withdrawals_count)
            {
                expected.push_withdrawal(hash, state.clone(), touched_keys);
                post_states.push(state);
            }
            let deposits = deposits.iter().take(deposits_count).cloned().collect();
            let deposit_states: Vec<_> =
                { deposits_state.clone().into_iter().take(deposits_count) }.collect();
            let deposit_touched_keys =
                { deposits_touch_keys.clone().into_iter().take(deposits_count) }.collect();

            post_states.extend(deposit_states.clone());
            let txs_prev_state_checkpoint = {
                let state = post_states.last().unwrap();
                calculate_state_checkpoint(&state.merkle_root().unpack(), state.count().unpack())
            };
            expected.push_deposits(
                deposits,
                deposit_states,
                deposit_touched_keys,
                txs_prev_state_checkpoint,
            );

            for (hash, state) in { txs.clone().into_iter() }
                .zip(txs_state.clone())
                .take(txs_count)
            {
                expected.push_tx(hash, state.clone());
                post_states.push(state);
            }

            expected.set_finalized_custodian_capacity(finalized_custodians.clone());

            (expected, post_states.last().unwrap().to_owned())
        };

        // Retry count 1, should remove half of packaged state changes
        let total =
            mem_block.withdrawals().len() + mem_block.deposits().len() + mem_block.txs().len();
        let remain = total.shr(1);
        assert!(remain > 0usize);

        let output_param = OutputParam { retry_count: 1 };
        let (mem_block_out, post_block_state) =
            MemPool::package_mem_block(&mem_block, &output_param);

        let (withdrawals_count, deposits_count, txs_count) =
            repackage_count(&mem_block, &output_param);
        assert!(txs_count > 0);

        let (expected_block, expected_post_state) =
            repackage(withdrawals_count, deposits_count, txs_count);

        assert_eq!(mem_block_out.cmp(&expected_block), MemBlockCmp::Same);
        assert_eq!(post_block_state, expected_post_state);

        // Retry count 2
        let remain = total.shr(2);
        assert!(remain > 0usize);

        let output_param = OutputParam { retry_count: 2 };
        let (mem_block_out, post_block_state) =
            MemPool::package_mem_block(&mem_block, &output_param);

        let (withdrawals_count, deposits_count, txs_count) =
            repackage_count(&mem_block, &output_param);
        assert!(txs_count > 0);

        let (expected_block, expected_post_state) =
            repackage(withdrawals_count, deposits_count, txs_count);

        assert_eq!(mem_block_out.cmp(&expected_block), MemBlockCmp::Same);
        assert_eq!(post_block_state, expected_post_state);

        // Retry count 3
        let remain = total.shr(3);
        assert!(remain > 0usize);

        let output_param = OutputParam { retry_count: 3 };
        let (mem_block_out, post_block_state) =
            MemPool::package_mem_block(&mem_block, &output_param);

        let (withdrawals_count, deposits_count, txs_count) =
            repackage_count(&mem_block, &output_param);
        assert_eq!(txs_count, 0);
        assert!(deposits_count > 0);

        let (expected_block, expected_post_state) =
            repackage(withdrawals_count, deposits_count, txs_count);

        assert_eq!(mem_block_out.cmp(&expected_block), MemBlockCmp::Same);
        assert_eq!(post_block_state, expected_post_state);

        // Retry count 4 ~ 9
        for retry_count in 4..=9 {
            let remain = total.shr(retry_count);
            assert!(remain > 0usize);

            let output_param = OutputParam { retry_count };
            let (mem_block_out, post_block_state) =
                MemPool::package_mem_block(&mem_block, &output_param);

            let (withdrawals_count, deposits_count, txs_count) =
                repackage_count(&mem_block, &output_param);
            assert_eq!(txs_count, 0);
            assert_eq!(deposits_count, 0);
            assert!(withdrawals_count > 0);

            let (expected_block, expected_post_state) =
                repackage(withdrawals_count, deposits_count, txs_count);

            assert_eq!(mem_block_out.cmp(&expected_block), MemBlockCmp::Same);
            assert_eq!(post_block_state, expected_post_state);
        }

        // Retry count 10
        let remain = total.shr(10);
        assert_eq!(remain, 0usize);

        let output_param = OutputParam { retry_count: 10 };
        let (mem_block_out, post_block_state) =
            MemPool::package_mem_block(&mem_block, &output_param);

        let (withdrawals_count, deposits_count, txs_count) =
            repackage_count(&mem_block, &output_param);
        assert_eq!(txs_count, 0);
        assert_eq!(deposits_count, 0);
        assert_eq!(withdrawals_count, 1);

        // Should package at least one
        let (expected_block, expected_post_state) =
            repackage(withdrawals_count, deposits_count, txs_count);

        assert_eq!(mem_block_out.cmp(&expected_block), MemBlockCmp::Same);
        assert_eq!(post_block_state, expected_post_state);
    }

    fn random_hash() -> H256 {
        rand::random()
    }

    fn random_state() -> AccountMerkleState {
        AccountMerkleState::new_builder()
            .merkle_root(random_hash().pack())
            .count(rand::random::<u32>().pack())
            .build()
    }
}
