// Copyright © Aptos Foundation

use crate::{
    block_executor::BlockAptosVM,
    sharded_block_executor::{
        block_executor_client::BlockExecutorClient,
        counters::SHARDED_BLOCK_EXECUTION_SECONDS,
        cross_shard_client::{CrossShardCommitReceiver, CrossShardCommitSender},
        cross_shard_state_view::CrossShardStateView,
        messages::CrossShardMsg,
    },
};
use aptos_block_partitioner::sharded_block_partitioner::MAX_ALLOWED_PARTITIONING_ROUNDS;
use aptos_logger::{info, trace};
use aptos_metrics_core::{
    exponential_buckets, register_histogram_vec, register_int_counter_vec, HistogramVec,
    IntCounterVec,
};
use aptos_state_view::StateView;
use aptos_types::{
    block_executor::partitioner::{
        BlockExecutorTransactions, ShardId, SubBlock, SubBlocksForShard,
    },
    transaction::{Transaction, TransactionOutput},
};
use aptos_vm_logging::disable_speculative_logging;
use futures::{channel::oneshot, executor::block_on};
use move_core_types::vm_status::VMStatus;
use once_cell::sync::Lazy;
use std::{
    collections::HashSet,
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
};
use std::cmp::min;

pub struct ShardedExecutorClient {
    num_shards: ShardId,
    shard_id: ShardId,
    executor_thread_pool: Arc<rayon::ThreadPool>,
    // The receivers of cross shard messages from other shards per round.
    message_rxs: Arc<Vec<Mutex<Receiver<CrossShardMsg>>>>,
    // The senders of cross-shard messages to other shards per round.
    message_txs: Arc<Vec<Vec<Mutex<Sender<CrossShardMsg>>>>>,
}

impl ShardedExecutorClient {
    pub fn new(
        num_shards: ShardId,
        shard_id: ShardId,
        num_threads: usize,
        message_rxs: Vec<Receiver<CrossShardMsg>>,
        message_txs: Vec<Vec<Sender<CrossShardMsg>>>,
    ) -> Self {
        let executor_thread_pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                // We need two extra threads for the cross-shard commit receiver and the thread
                // that is blocked on waiting for execute block to finish.
                .num_threads(num_threads + 2)
                .build()
                .unwrap(),
        );
        Self {
            num_shards,
            shard_id,
            executor_thread_pool,
            message_rxs: Arc::new(message_rxs.into_iter().map(Mutex::new).collect()),
            message_txs: Arc::new(
                message_txs
                    .into_iter()
                    .map(|inner_vec| inner_vec.into_iter().map(Mutex::new).collect())
                    .collect(),
            ),
        }
    }

    pub fn create_sharded_executor_clients(
        num_shards: usize,
        num_threads: Option<usize>,
    ) -> Vec<Self> {
        let num_threads = num_threads
            .unwrap_or_else(|| (num_cpus::get() as f64 / num_shards as f64).ceil() as usize);
        let mut cross_shard_msg_txs = vec![];
        let mut cross_shard_msg_rxs = vec![];
        for _ in 0..num_shards {
            let mut current_shard_msg_txs = vec![];
            let mut current_shard_msg_rxs = vec![];
            for _ in 0..MAX_ALLOWED_PARTITIONING_ROUNDS {
                let (messages_tx, messages_rx) = std::sync::mpsc::channel();
                current_shard_msg_txs.push(messages_tx);
                current_shard_msg_rxs.push(messages_rx);
            }
            cross_shard_msg_txs.push(current_shard_msg_txs);
            cross_shard_msg_rxs.push(current_shard_msg_rxs);
        }
        cross_shard_msg_rxs
            .into_iter()
            .enumerate()
            .map(|(shard_id, rxs)| {
                let num_threads = match std::env::var("SHARDED_PARTITIONER__MERGE_LAST_ROUND") {
                    Ok(v) if v.as_str() == "1" => {
                        if shard_id == num_shards - 1 {
                            num_threads * num_shards
                        } else {
                            num_threads
                        }
                    },
                    _ => num_threads,
                };
                Self::new(
                    num_shards as ShardId,
                    shard_id as ShardId,
                    num_threads,
                    rxs,
                    cross_shard_msg_txs.clone(),
                )
            })
            .collect()
    }

    fn create_cross_shard_state_view<'a, S: StateView + Sync + Send>(
        &self,
        base_view: &'a S,
        sub_block: &SubBlock<Transaction>,
    ) -> CrossShardStateView<'a, S> {
        let mut cross_shard_state_key = HashSet::new();
        for (_local_tid, txn) in sub_block.transactions.iter().enumerate() {
            for (_src_txn, storage_locations) in txn.cross_shard_dependencies.required_edges_iter()
            {
                for storage_location in storage_locations {
                    let state_key = storage_location.clone().into_state_key();
                    // let key_str = state_key.hash().to_hex();
                    // info!("CCSSV, dst_shard_id={}, dst_txn_idx={}, src_shard_id={}, src_txn_idx={}, key={}", self.shard_id, sub_block.start_index + local_tid, src_txn.shard_id, src_txn.txn_index, key_str);
                    cross_shard_state_key.insert(state_key);
                }
            }
        }
        CrossShardStateView::new(self.shard_id, cross_shard_state_key, base_view)
    }

    fn execute_sub_block<S: StateView + Sync + Send>(
        &self,
        sub_block: SubBlock<Transaction>,
        round: usize,
        state_view: &S,
        concurrency_level: usize,
        maybe_block_gas_limit: Option<u64>,
    ) -> Result<Vec<TransactionOutput>, VMStatus> {
        let label_shard_id = format!("{}", self.shard_id);
        let label_round_id = format!("{}", round);
        APTOS_SUB_BLOCK_SIZES
            .with_label_values(&[label_shard_id.as_str(), label_round_id.as_str()])
            .inc_by(sub_block.num_txns() as u64);
        let _timer = APTOS_SUB_BLOCK_EXECUTION_SECONDS
            .with_label_values(&[label_shard_id.as_str(), label_round_id.as_str()])
            .start_timer();
        info!(
            "executing sub block for shard {} and round {} with concurrency_level={}, thread_pool_size={}",
            self.shard_id,
            round,
            concurrency_level,
            self.executor_thread_pool.current_num_threads()
        );
        let cross_shard_commit_sender =
            CrossShardCommitSender::new(self.shard_id, self.message_txs.clone(), &sub_block);

        let (callback, callback_receiver) = oneshot::channel();

        let message_rxs = self.message_rxs.clone();
        let self_message_tx = Arc::new(Mutex::new(
            self.message_txs[self.shard_id][round]
                .lock()
                .unwrap()
                .clone(),
        ));
        let cross_shard_state_view =
            Arc::new(self.create_cross_shard_state_view(state_view, &sub_block));
        let cross_shard_state_view_clone1 = cross_shard_state_view.clone();
        self.executor_thread_pool.scope(|s| {
            s.spawn(move |_| {
                if round != 0 {
                    // If this is not the first round, start the cross-shard commit receiver.
                    // this is a bit ugly, we can get rid of this when we have round number
                    // information in the cross shard dependencies.
                    CrossShardCommitReceiver::start(
                        cross_shard_state_view_clone1,
                        &message_rxs[round].lock().unwrap(),
                    );
                }
            });
            s.spawn(move |_| {
                disable_speculative_logging();
                let ret = BlockAptosVM::execute_block(
                    self.executor_thread_pool.clone(),
                    BlockExecutorTransactions::Unsharded(sub_block.into_txns()),
                    cross_shard_state_view.as_ref(),
                    concurrency_level,
                    maybe_block_gas_limit,
                    Some(cross_shard_commit_sender),
                );
                // Send a stop command to the cross-shard commit receiver.
                if round != 0 {
                    self_message_tx
                        .lock()
                        .unwrap()
                        .send(CrossShardMsg::StopMsg)
                        .unwrap();
                }
                callback.send(ret).unwrap();
            });
        });
        let ret = block_on(callback_receiver).unwrap();
        trace!(
            "finished executing sub block for shard {} and round {}",
            self.shard_id,
            round
        );
        ret
    }
}

const OPTIMAL_BLOCKSTM_CONCURRENCY_LEVEL: usize = 32;

impl BlockExecutorClient for ShardedExecutorClient {
    fn execute_block<S: StateView + Sync + Send>(
        &self,
        transactions: SubBlocksForShard<Transaction>,
        state_view: &S,
        concurrency_level: usize,
        maybe_block_gas_limit: Option<u64>,
    ) -> Result<Vec<Vec<TransactionOutput>>, VMStatus> {
        let mut result = vec![];
        let sub_blocks = transactions.into_sub_blocks();
        let num_rounds = sub_blocks.len();
        for (round, sub_block) in sub_blocks.into_iter().enumerate() {
            let _timer = SHARDED_BLOCK_EXECUTION_SECONDS
                .with_label_values(&[&self.shard_id.to_string(), &round.to_string()])
                .start_timer();
            // A hacky way to ensure last-round txns are executed in a single large BlockSTM.
            // TODO: let the partitioner leave a flag in the special sub-block instead.
            let modified_concurrency_level = match std::env::var("SHARDED_PARTITIONER__MERGE_LAST_ROUND") {
                Ok(v) if v.as_str() == "1" => {
                    if round == num_rounds - 1 {
                        if self.shard_id == self.num_shards - 1 {
                            min(concurrency_level * self.num_shards, OPTIMAL_BLOCKSTM_CONCURRENCY_LEVEL)
                        } else {
                            1
                        }
                    } else {
                        concurrency_level
                    }
                },
                _ => concurrency_level
            };
            info!("executing sub block for shard {} and round {}, number of txns {} with concurrency level {}.", self.shard_id, round, sub_block.transactions.len(), modified_concurrency_level);

            result.push(self.execute_sub_block(
                sub_block,
                round,
                state_view,
                modified_concurrency_level,
                maybe_block_gas_limit,
            )?);
        }
        Ok(result)
    }
}

pub static APTOS_SUB_BLOCK_EXECUTION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        // metric name
        "aptos_sub_block_execution_seconds",
        // metric description
        "foo",
        &["shard_id", "round_id"],
        exponential_buckets(/*start=*/ 1e-3, /*factor=*/ 2.0, /*count=*/ 20).unwrap(),
    )
    .unwrap()
});

pub static APTOS_SUB_BLOCK_SIZES: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!(
        // metric name
        "aptos_sub_block_sizes",
        // metric description
        "foo",
        &["shard_id", "round_id"],
    )
    .unwrap()
});
