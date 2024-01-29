// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    account_generator::AccountGeneratorCreator, accounts_pool_wrapper::AccountsPoolWrapperCreator,
    call_custom_modules::CustomModulesDelegationGeneratorCreator,
    entry_points::EntryPointTransactionGenerator, EntryPoints, ObjectPool,
    ReliableTransactionSubmitter, TransactionGenerator, TransactionGeneratorCreator, WorkflowKind,
    WorkflowProgress,
    tournament_generator::{TournamentStartNewRoundTransactionGenerator, TournamentMovePlayersToRoundTransactionGenerator}, reduced_batch_wrapper::ReducedBatchWrapperTransactionGeneratorCreator,
};
use aptos_logger::{info, sample, sample::SampleRate};
use aptos_sdk::{
    transaction_builder::TransactionFactory,
    types::{transaction::SignedTransaction, LocalAccount},
};
use std::{
    cmp,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
enum StageTracking {
    // stage is externally modified
    ExternallySet(Arc<AtomicUsize>),
    // we move to a next stage when all accounts have finished with the current stage
    WhenDone {
        stage_counter: Arc<AtomicUsize>,
        stage_start_time: Arc<AtomicU64>,
        delay_between_stages: Duration,
    },
}

impl StageTracking {
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn load_current_stage(&self) -> Option<usize> {
        match self {
            StageTracking::ExternallySet(stage_counter) => {
                Some(stage_counter.load(Ordering::Relaxed))
            },
            StageTracking::WhenDone {
                stage_counter,
                stage_start_time,
                ..
            } => {
                if stage_start_time.load(Ordering::Relaxed) > Self::current_timestamp() {
                    None
                } else {
                    Some(stage_counter.load(Ordering::Relaxed))
                }
            },
        }
    }
}

/// Generator allowing for multi-stage workflows.
/// List of generators are passed:
/// gen_0, gen_1, ... gen_n
/// and on list of account pools, each representing accounts in between two stages:
/// pool_0, pool_1, ... pool_n-1
///
/// pool_i is filled by gen_i, and consumed by gen_i+1, and so there is one less pools than generators.
///
/// We start with stage 0, which calls gen_0 pool_per_stage times, which populates pool_0 with accounts.
///
/// After that, in stage 1, we call gen_1, which consumes accounts from pool_0, and moves them to pool_1.
/// We do this until pool_0 is empty.
///
/// We proceed, until in the last stage - stage n - calls gen_n, which consumes accounts from pool_n-1.
///
/// There are two modes on when to move to the next stage:
/// - WhenDone means as soon as pool_i is empty, we move to stage i+1
/// - ExternallySet means we wait for external signal to move to next stage, and we stop creating transactions
///   until we receive it (or will move early if pool hasn't been consumed yet)
///
/// Use WorkflowTxnGeneratorCreator::create_workload to create this generator.
struct WorkflowTxnGenerator {
    stage: StageTracking,
    generators: Vec<Box<dyn TransactionGenerator>>,
    pool_per_stage: Vec<Arc<ObjectPool<LocalAccount>>>,
    num_for_first_stage: usize,
    // Internal counter, so multiple workers (WorkflowTxnGenerator) can coordinate how many times to execute the first stage
    completed_for_first_stage: Arc<AtomicUsize>,
}

impl WorkflowTxnGenerator {
    fn new(
        stage: StageTracking,
        generators: Vec<Box<dyn TransactionGenerator>>,
        pool_per_stage: Vec<Arc<ObjectPool<LocalAccount>>>,
        num_for_first_stage: usize,
        completed_for_first_stage: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            stage,
            generators,
            pool_per_stage,
            num_for_first_stage,
            completed_for_first_stage,
        }
    }
}

impl TransactionGenerator for WorkflowTxnGenerator {
    fn generate_transactions(
        &mut self,
        account: &LocalAccount,
        mut num_to_create: usize,
    ) -> Vec<SignedTransaction> {
        assert_ne!(num_to_create, 0);
        let stage = match self.stage.load_current_stage() {
            Some(stage) => stage,
            None => {
                sample!(
                    SampleRate::Duration(Duration::from_secs(2)),
                    info!("Waiting for delay before next stage");
                );
                return Vec::new();
            },
        };

        if stage == 0 {
            // We can treat completed_for_first_stage as a stream of indices [0, +inf),
            // where we want to execute only first num_for_first_stage (i.e. [0, num_for_first_stage) )
            // So here we grab num_to_create "indices" from completed_for_first_stage counter,
            // and then skip those that are in [num_for_first_stage, +inf) range.
            let prev = self
                .completed_for_first_stage
                .fetch_add(num_to_create, Ordering::Relaxed);
            num_to_create = cmp::min(num_to_create, self.num_for_first_stage.saturating_sub(prev));
        }
        // if stage is not 0, then grabing from the pool itself, inside of the generator.generate_transactions
        // acts as coordinator, as it will generate as many transactions as number of accounts it could grab from the pool.

        match &self.stage {
            StageTracking::WhenDone {
                stage_counter,
                stage_start_time,
                delay_between_stages,
            } => {
                if stage == 0 {
                    if num_to_create == 0 {
                        info!("TransactionGenerator Workflow: Stage 0 is full with {} accounts, moving to stage 1", self.pool_per_stage.first().unwrap().len());
                        stage_start_time.store(
                            StageTracking::current_timestamp() + delay_between_stages.as_secs(),
                            Ordering::Relaxed,
                        );
                        let _ = stage_counter.compare_exchange(
                            0,
                            1,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                        return Vec::new();
                    }
                } else if stage < self.pool_per_stage.len()
                    && self.pool_per_stage.get(stage - 1).unwrap().len() == 0
                {
                    info!("TransactionGenerator Workflow: Stage {} has consumed all accounts, moving to stage {}", stage, stage + 1);
                    stage_start_time.store(
                        StageTracking::current_timestamp() + delay_between_stages.as_secs(),
                        Ordering::Relaxed,
                    );
                    let _ = stage_counter.compare_exchange(
                        stage,
                        stage + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                    return Vec::new();
                }
            },
            StageTracking::ExternallySet(_) => {
                if stage == 0 && num_to_create == 0 {
                    return Vec::new();
                }
            },
        }

        sample!(
            SampleRate::Duration(Duration::from_secs(2)),
            info!("Cur stage: {}, pool sizes: {:?}", stage, self.pool_per_stage.iter().map(|p| p.len()).collect::<Vec<_>>());
        );

        let result = if let Some(generator) = self.generators.get_mut(stage) {
            generator.generate_transactions(account, num_to_create)
        } else {
            Vec::new()
        };

        result
    }
}

pub struct WorkflowTxnGeneratorCreator {
    stage: StageTracking,
    creators: Vec<Box<dyn TransactionGeneratorCreator>>,
    pool_per_stage: Vec<Arc<ObjectPool<LocalAccount>>>,
    num_for_first_stage: usize,
    completed_for_first_stage: Arc<AtomicUsize>,
}

impl WorkflowTxnGeneratorCreator {
    fn new(
        stage: StageTracking,
        creators: Vec<Box<dyn TransactionGeneratorCreator>>,
        pool_per_stage: Vec<Arc<ObjectPool<LocalAccount>>>,
        num_for_first_stage: usize,
    ) -> Self {
        Self {
            stage,
            creators,
            pool_per_stage,
            num_for_first_stage,
            completed_for_first_stage: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn create_workload(
        workflow_kind: WorkflowKind,
        txn_factory: TransactionFactory,
        init_txn_factory: TransactionFactory,
        root_account: &mut LocalAccount,
        txn_executor: &dyn ReliableTransactionSubmitter,
        num_modules: usize,
        initial_account_pool: Option<Arc<ObjectPool<LocalAccount>>>,
        cur_phase: Arc<AtomicUsize>,
        progress_type: WorkflowProgress,
    ) -> Self {
        let stage_tracking = match progress_type {
            WorkflowProgress::MoveByPhases => StageTracking::ExternallySet(cur_phase),
            WorkflowProgress::WhenDone {
                delay_between_stages_s,
            } => StageTracking::WhenDone {
                stage_counter: Arc::new(AtomicUsize::new(0)),
                stage_start_time: Arc::new(AtomicU64::new(0)),
                delay_between_stages: Duration::from_secs(delay_between_stages_s),
            },
        };
        println!(
            "Creating workload with stage tracking: {:?}",
            match &stage_tracking {
                StageTracking::ExternallySet(_) => "ExternallySet",
                StageTracking::WhenDone { .. } => "WhenDone",
            }
        );
        match workflow_kind {
            WorkflowKind::CreateThenMint {
                count,
                creation_balance,
            } => {
                let created_pool = Arc::new(ObjectPool::new());
                let minted_pool = Arc::new(ObjectPool::new());
                let entry_point = EntryPoints::TokenV2AmbassadorMint;

                let creators: Vec<Box<dyn TransactionGeneratorCreator>> = vec![
                    Box::new(AccountGeneratorCreator::new(
                        txn_factory.clone(),
                        None,
                        Some(created_pool.clone()),
                        count,
                        creation_balance,
                    )),
                    Box::new(AccountsPoolWrapperCreator::new(
                        Box::new(
                            CustomModulesDelegationGeneratorCreator::new(
                                txn_factory.clone(),
                                init_txn_factory.clone(),
                                root_account,
                                txn_executor,
                                num_modules,
                                entry_point.package_name(),
                                &mut EntryPointTransactionGenerator { entry_point },
                            )
                            .await,
                        ),
                        created_pool.clone(),
                        Some(minted_pool.clone()),
                    )),
                ];
                Self::new(
                    stage_tracking,
                    creators,
                    vec![created_pool, minted_pool],
                    count,
                )
            },
            WorkflowKind::Tournament { num_players, join_batch } => {
                let create_accounts = initial_account_pool.is_none();
                let created_pool = initial_account_pool.unwrap_or(Arc::new(ObjectPool::new()));
                let player_setup_pool = Arc::new(ObjectPool::new());
                let round_created_pool = Arc::new(ObjectPool::new());
                let in_round_pool = Arc::new(ObjectPool::new());
                let finished_pool = Arc::new(ObjectPool::new());

                let mut packages = CustomModulesDelegationGeneratorCreator::publish_package(
                    init_txn_factory.clone(),
                    root_account,
                    txn_executor,
                    num_modules,
                    EntryPoints::TournamentSetupPlayer.package_name(),
                    Some(1000_000_00000000)
                ).await;

                let tournament_setup_player_worker =  CustomModulesDelegationGeneratorCreator::create_worker(init_txn_factory.clone(), root_account, txn_executor, &mut packages, &mut EntryPointTransactionGenerator {
                    entry_point: EntryPoints::TournamentSetupPlayer,
                }).await;
                let tournament_setup_round_worker =  CustomModulesDelegationGeneratorCreator::create_worker(init_txn_factory.clone(), root_account, txn_executor, &mut packages, &mut TournamentStartNewRoundTransactionGenerator::new(
                    player_setup_pool.clone(),
                    round_created_pool.clone(),
                )).await;
                let tournament_move_players_to_round_worker =  CustomModulesDelegationGeneratorCreator::create_worker(init_txn_factory.clone(), root_account, txn_executor, &mut packages, &mut TournamentMovePlayersToRoundTransactionGenerator::new(
                    round_created_pool.clone(),
                    in_round_pool.clone(),
                    join_batch,
                )).await;
                let tournament_game_play_worker =  CustomModulesDelegationGeneratorCreator::create_worker(init_txn_factory.clone(), root_account, txn_executor, &mut packages, &mut EntryPointTransactionGenerator {
                    entry_point: EntryPoints::TournamentGamePlay,
                }).await;

                let packages = Arc::new(packages);

                let mut creators: Vec<Box<dyn TransactionGeneratorCreator>> = vec![];
                if create_accounts {
                    creators.push(
                        Box::new(AccountGeneratorCreator::new(
                            txn_factory.clone(),
                            None,
                            Some(created_pool.clone()),
                            num_players,
                            // 0.04 APT
                            4_000_000,
                        ))
                    );
                }
                creators.push(
                    Box::new(
                        AccountsPoolWrapperCreator::new(
                            Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                                txn_factory.clone(),
                                packages.clone(),
                                tournament_setup_player_worker,
                            )),
                            created_pool.clone(),
                            Some(player_setup_pool.clone()),
                        )
                    ),
                );
                creators.push(
                    Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                        txn_factory.clone(),
                        packages.clone(),
                        tournament_setup_round_worker,
                    )),
                );
                creators.push(
                    Box::new(
                        // expensive batched transactions, reduce batch size
                        ReducedBatchWrapperTransactionGeneratorCreator::new(
                            1,
                            Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                                txn_factory.clone().with_max_gas_amount(1000000),
                                packages.clone(),
                                tournament_move_players_to_round_worker,
                            )),
                        ),
                    )
                );
                creators.push(
                    Box::new(
                        AccountsPoolWrapperCreator::new(
                            Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                                txn_factory.clone(),
                                packages.clone(),
                                tournament_game_play_worker,
                            )),
                            in_round_pool.clone(),
                            Some(finished_pool.clone()),
                        )
                    ),
                );

                let pool_per_stage = if create_accounts {
                    vec![created_pool, player_setup_pool, round_created_pool, in_round_pool]
                } else {
                    vec![player_setup_pool, round_created_pool, in_round_pool]
                };

                Self::new(stage_tracking, creators, pool_per_stage, num_players)
            }
        }
    }
}

impl TransactionGeneratorCreator for WorkflowTxnGeneratorCreator {
    fn create_transaction_generator(&self) -> Box<dyn TransactionGenerator> {
        Box::new(WorkflowTxnGenerator::new(
            self.stage.clone(),
            self.creators
                .iter()
                .map(|c| c.create_transaction_generator())
                .collect(),
            self.pool_per_stage.clone(),
            self.num_for_first_stage,
            self.completed_for_first_stage.clone(),
        ))
    }
}
