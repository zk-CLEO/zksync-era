use assert_matches::assert_matches;
use db_test_macro::db_test;
use itertools::Itertools;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use std::{future::Future, ops, panic, path::Path, time::Duration};

use zksync_config::configs::chain::OperationsManagerConfig;

use zksync_config::DBConfig;
use zksync_contracts::BaseSystemContracts;
use zksync_dal::{ConnectionPool, StorageProcessor};
use zksync_health_check::{CheckHealth, HealthStatus};
use zksync_object_store::{ObjectStore, ObjectStoreFactory};
use zksync_types::{
    block::{BlockGasCount, L1BatchHeader, MiniblockHeader},
    proofs::PrepareBasicCircuitsJob,
    system_contracts::get_system_smart_contracts,
    AccountTreeId, Address, L1BatchNumber, L2ChainId, MiniblockNumber, StorageKey, StorageLog,
    H256,
};
use zksync_utils::{miniblock_hash, u32_to_h256};

use super::{MetadataCalculator, MetadataCalculatorConfig, MetadataCalculatorModeConfig};
use crate::genesis::{ensure_genesis_state, GenesisParams};

const RUN_TIMEOUT: Duration = Duration::from_secs(15);

async fn run_with_timeout<T, F>(timeout: Duration, action: F) -> T
where
    F: Future<Output = T>,
{
    let timeout_handle = tokio::time::timeout(timeout, action);
    match timeout_handle.await {
        Ok(res) => res,
        Err(_) => panic!("timed out waiting for metadata calculator"),
    }
}

#[db_test]
async fn genesis_creation(pool: ConnectionPool, prover_pool: ConnectionPool) {
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");

    let (calculator, _) = setup_calculator(temp_dir.path(), &pool).await;
    run_calculator(calculator, pool.clone(), prover_pool).await;
    let (calculator, _) = setup_calculator(temp_dir.path(), &pool).await;
    assert_eq!(
        calculator.updater.tree().next_l1_batch_number(),
        L1BatchNumber(1)
    );
}

// TODO (SMA-1726): Restore tests for tree backup mode

#[db_test]
async fn basic_workflow(pool: ConnectionPool, prover_pool: ConnectionPool) {
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");

    let (calculator, object_store) = setup_calculator(temp_dir.path(), &pool).await;
    reset_db_state(&pool, 1).await;
    run_calculator(calculator, pool.clone(), prover_pool).await;

    let job: PrepareBasicCircuitsJob = object_store.get(L1BatchNumber(1)).await.unwrap();
    assert!(job.next_enumeration_index() > 0);
    let merkle_paths: Vec<_> = job.clone().into_merkle_paths().collect();
    assert!(!merkle_paths.is_empty() && merkle_paths.len() <= 100);
    // ^ The exact values depend on ops in genesis block
    assert!(merkle_paths.iter().all(|log| log.is_write));

    let (calculator, _) = setup_calculator(temp_dir.path(), &pool).await;
    assert_eq!(
        calculator.updater.tree().next_l1_batch_number(),
        L1BatchNumber(2)
    );
}

#[db_test]
async fn status_receiver_has_correct_states(pool: ConnectionPool, prover_pool: ConnectionPool) {
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");

    let (mut calculator, _) = setup_calculator(temp_dir.path(), &pool).await;
    let tree_health_check = calculator.tree_health_check();
    assert_eq!(tree_health_check.name(), "tree");
    let health = tree_health_check.check_health().await;
    assert_matches!(health.status(), HealthStatus::NotReady);

    let other_tree_health_check = calculator.tree_health_check();
    assert_eq!(other_tree_health_check.name(), "tree");
    let health = other_tree_health_check.check_health().await;
    assert_matches!(health.status(), HealthStatus::NotReady);

    reset_db_state(&pool, 1).await;
    let (stop_sx, stop_rx) = watch::channel(false);
    let (delay_sx, mut delay_rx) = mpsc::unbounded_channel();
    calculator.delayer.delay_notifier = delay_sx;

    let calculator_handle = tokio::spawn(calculator.run(pool, prover_pool, stop_rx));
    delay_rx.recv().await.unwrap();
    assert_eq!(
        tree_health_check.check_health().await.status(),
        HealthStatus::Ready
    );
    assert_eq!(
        other_tree_health_check.check_health().await.status(),
        HealthStatus::Ready
    );

    stop_sx.send(true).unwrap();
    tokio::time::timeout(RUN_TIMEOUT, calculator_handle)
        .await
        .expect("timed out waiting for calculator")
        .unwrap();
    assert_eq!(
        tree_health_check.check_health().await.status(),
        HealthStatus::ShutDown
    );
    assert_eq!(
        other_tree_health_check.check_health().await.status(),
        HealthStatus::ShutDown
    );
}

#[db_test]
async fn multi_block_workflow(pool: ConnectionPool, prover_pool: ConnectionPool) {
    // Run all transactions as a single block
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
    let (calculator, _) = setup_calculator(temp_dir.path(), &pool).await;
    reset_db_state(&pool, 1).await;
    let root_hash = run_calculator(calculator, pool.clone(), prover_pool.clone()).await;

    // Run the same transactions as multiple blocks
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
    let (calculator, object_store) = setup_calculator(temp_dir.path(), &pool).await;
    reset_db_state(&pool, 10).await;
    let multi_block_root_hash = run_calculator(calculator, pool, prover_pool).await;
    assert_eq!(multi_block_root_hash, root_hash);

    let mut prev_index = None;
    for block_number in 1..=10 {
        let block_number = L1BatchNumber(block_number);
        let job: PrepareBasicCircuitsJob = object_store.get(block_number).await.unwrap();
        let next_enumeration_index = job.next_enumeration_index();
        let merkle_paths: Vec<_> = job.into_merkle_paths().collect();
        assert!(!merkle_paths.is_empty() && merkle_paths.len() <= 10);

        if let Some(prev_index) = prev_index {
            assert_eq!(next_enumeration_index, prev_index + 1);
        }
        let max_leaf_index_in_block = merkle_paths
            .iter()
            .filter_map(|log| log.first_write.then_some(log.leaf_enumeration_index))
            .max();
        prev_index = max_leaf_index_in_block.or(prev_index);
    }
}

#[db_test]
async fn running_metadata_calculator_with_additional_blocks(
    pool: ConnectionPool,
    prover_pool: ConnectionPool,
) {
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
    let calculator = setup_lightweight_calculator(temp_dir.path(), &pool).await;
    reset_db_state(&pool, 5).await;
    run_calculator(calculator, pool.clone(), prover_pool.clone()).await;

    let mut calculator = setup_lightweight_calculator(temp_dir.path(), &pool).await;
    let (stop_sx, stop_rx) = watch::channel(false);
    let (delay_sx, mut delay_rx) = mpsc::unbounded_channel();
    calculator.delayer.delay_notifier = delay_sx;

    let calculator_handle =
        tokio::spawn(calculator.run(pool.clone(), prover_pool.clone(), stop_rx));
    // Wait until the calculator has processed initial blocks.
    let (next_l1_batch, _) = tokio::time::timeout(RUN_TIMEOUT, delay_rx.recv())
        .await
        .expect("metadata calculator timed out processing initial blocks")
        .unwrap();
    assert_eq!(next_l1_batch, L1BatchNumber(6));

    // Add some new blocks to the storage.
    let new_logs = gen_storage_logs(100..200, 10);
    extend_db_state(&mut pool.access_storage().await, new_logs).await;

    // Wait until these blocks are processed. The calculator may have spurious delays,
    // thus we wait in a loop.
    let updated_root_hash = loop {
        let (next_l1_batch, root_hash) = tokio::time::timeout(RUN_TIMEOUT, delay_rx.recv())
            .await
            .expect("metadata calculator shut down prematurely")
            .unwrap();
        if next_l1_batch == L1BatchNumber(16) {
            stop_sx.send(true).unwrap(); // Shut down the calculator.
            break root_hash;
        }
    };
    tokio::time::timeout(RUN_TIMEOUT, calculator_handle)
        .await
        .expect("timed out waiting for calculator")
        .unwrap();

    // Switch to the full tree. It should pick up from the same spot and result in the same tree root hash.
    let (calculator, _) = setup_calculator(temp_dir.path(), &pool).await;
    let root_hash_for_full_tree = run_calculator(calculator, pool, prover_pool).await;
    assert_eq!(root_hash_for_full_tree, updated_root_hash);
}

#[db_test]
async fn shutting_down_calculator(pool: ConnectionPool, prover_pool: ConnectionPool) {
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
    let (db_config, mut operation_config) = create_config(temp_dir.path());
    operation_config.delay_interval = 30_000; // ms; chosen to be larger than `RUN_TIMEOUT`

    let calculator = setup_calculator_with_options(
        &db_config,
        &operation_config,
        &pool,
        MetadataCalculatorModeConfig::Lightweight,
    )
    .await;

    reset_db_state(&pool, 5).await;

    let (stop_sx, stop_rx) = watch::channel(false);
    let calculator_task = tokio::spawn(calculator.run(pool, prover_pool, stop_rx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop_sx.send_replace(true);
    run_with_timeout(RUN_TIMEOUT, calculator_task)
        .await
        .unwrap();
}

async fn test_postgres_backup_recovery(
    pool: ConnectionPool,
    prover_pool: ConnectionPool,
    sleep_between_batches: bool,
    insert_batch_without_metadata: bool,
) {
    let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
    let calculator = setup_lightweight_calculator(temp_dir.path(), &pool).await;
    reset_db_state(&pool, 5).await;
    run_calculator(calculator, pool.clone(), prover_pool.clone()).await;

    // Simulate recovery from a DB snapshot in which some newer L1 batches are erased.
    let last_batch_after_recovery = L1BatchNumber(3);
    let mut storage = pool.access_storage().await;
    let removed_batches = remove_l1_batches(&mut storage, last_batch_after_recovery).await;

    if insert_batch_without_metadata {
        let batches_without_metadata =
            remove_l1_batches(&mut storage, last_batch_after_recovery - 1).await;
        let [batch_without_metadata] = batches_without_metadata.as_slice() else {
            unreachable!()
        };
        // Re-insert the last batch without metadata immediately.
        storage
            .blocks_dal()
            .insert_l1_batch(batch_without_metadata, BlockGasCount::default())
            .await;
        insert_initial_writes_for_batch(&mut storage, batch_without_metadata.number).await;
    }
    drop(storage);

    let mut calculator = setup_lightweight_calculator(temp_dir.path(), &pool).await;
    let (stop_sx, stop_rx) = watch::channel(false);
    let (delay_sx, mut delay_rx) = mpsc::unbounded_channel();
    calculator.delayer.delay_notifier = delay_sx;

    let calculator_handle = tokio::spawn(calculator.run(pool.clone(), prover_pool, stop_rx));
    // Wait until the calculator has processed initial L1 batches.
    let (next_l1_batch, _) = tokio::time::timeout(RUN_TIMEOUT, delay_rx.recv())
        .await
        .expect("metadata calculator timed out after recovery")
        .unwrap();
    assert_eq!(next_l1_batch, last_batch_after_recovery + 1);

    // Re-insert L1 batches to the storage after recovery.
    let mut storage = pool.access_storage().await;
    for batch_header in &removed_batches {
        storage
            .blocks_dal()
            .insert_l1_batch(batch_header, BlockGasCount::default())
            .await;
        insert_initial_writes_for_batch(&mut storage, batch_header.number).await;
        if sleep_between_batches {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    drop(storage);

    // Wait until these batches are processed.
    loop {
        let (next_l1_batch, _) = tokio::time::timeout(RUN_TIMEOUT, delay_rx.recv())
            .await
            .expect("metadata calculator shut down prematurely")
            .unwrap();
        if next_l1_batch == L1BatchNumber(6) {
            stop_sx.send(true).unwrap(); // Shut down the calculator.
            break;
        }
    }
    tokio::time::timeout(RUN_TIMEOUT, calculator_handle)
        .await
        .expect("timed out waiting for calculator")
        .unwrap();
}

#[db_test]
async fn postgres_backup_recovery(pool: ConnectionPool, prover_pool: ConnectionPool) {
    test_postgres_backup_recovery(pool, prover_pool, false, false).await;
}

#[db_test]
async fn postgres_backup_recovery_with_delay_between_batches(
    pool: ConnectionPool,
    prover_pool: ConnectionPool,
) {
    test_postgres_backup_recovery(pool, prover_pool, true, false).await;
}

#[db_test]
async fn postgres_backup_recovery_with_excluded_metadata(
    pool: ConnectionPool,
    prover_pool: ConnectionPool,
) {
    test_postgres_backup_recovery(pool, prover_pool, false, true).await;
}

async fn setup_calculator(
    db_path: &Path,
    pool: &ConnectionPool,
) -> (MetadataCalculator, Box<dyn ObjectStore>) {
    let store_factory = &ObjectStoreFactory::mock();
    let (db_config, operation_manager) = create_config(db_path);
    let mode = MetadataCalculatorModeConfig::Full { store_factory };
    let calculator =
        setup_calculator_with_options(&db_config, &operation_manager, pool, mode).await;
    (calculator, store_factory.create_store().await)
}

async fn setup_lightweight_calculator(db_path: &Path, pool: &ConnectionPool) -> MetadataCalculator {
    let mode = MetadataCalculatorModeConfig::Lightweight;
    let (db_config, operation_config) = create_config(db_path);
    setup_calculator_with_options(&db_config, &operation_config, pool, mode).await
}

fn create_config(db_path: &Path) -> (DBConfig, OperationsManagerConfig) {
    let db_config = DBConfig {
        merkle_tree_path: path_to_string(&db_path.join("new")),
        backup_interval_ms: 0,
        ..DBConfig::from_env()
    };
    let operation_config = OperationsManagerConfig {
        delay_interval: 50, // ms
    };
    (db_config, operation_config)
}

async fn setup_calculator_with_options(
    db_config: &DBConfig,
    operation_config: &OperationsManagerConfig,
    pool: &ConnectionPool,
    mode: MetadataCalculatorModeConfig<'_>,
) -> MetadataCalculator {
    let calculator_config =
        MetadataCalculatorConfig::for_main_node(db_config, operation_config, mode);
    let metadata_calculator = MetadataCalculator::new(&calculator_config).await;

    let mut storage = pool.access_storage().await;
    if storage.blocks_dal().is_genesis_needed().await {
        let chain_id = L2ChainId(270);
        let base_system_contracts = BaseSystemContracts::load_from_disk();
        let system_contracts = get_system_smart_contracts();
        let first_validator = Address::repeat_byte(0x01);
        ensure_genesis_state(
            &mut storage,
            chain_id,
            &GenesisParams {
                first_validator,
                base_system_contracts,
                system_contracts,
            },
        )
        .await;
    }
    metadata_calculator
}

fn path_to_string(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

async fn run_calculator(
    mut calculator: MetadataCalculator,
    pool: ConnectionPool,
    prover_pool: ConnectionPool,
) -> H256 {
    let (stop_sx, stop_rx) = watch::channel(false);
    let (delay_sx, mut delay_rx) = mpsc::unbounded_channel();
    calculator.delayer.delay_notifier = delay_sx;
    let delayer_handle = tokio::spawn(async move {
        // Wait until the calculator has processed all initially available L1 batches,
        // then stop it via signal.
        let (_, root_hash) = delay_rx
            .recv()
            .await
            .expect("metadata calculator shut down prematurely");
        stop_sx.send(true).unwrap();
        root_hash
    });

    run_with_timeout(RUN_TIMEOUT, calculator.run(pool, prover_pool, stop_rx)).await;
    delayer_handle.await.unwrap()
}

pub(super) async fn reset_db_state(pool: &ConnectionPool, num_batches: usize) {
    let mut storage = pool.access_storage().await;
    // Drops all L1 batches (except the L1 batch with number 0) and their storage logs.
    storage
        .storage_logs_dal()
        .rollback_storage_logs(MiniblockNumber(0))
        .await;
    storage
        .blocks_dal()
        .delete_miniblocks(MiniblockNumber(0))
        .await;
    storage
        .blocks_dal()
        .delete_l1_batches(L1BatchNumber(0))
        .await;

    let logs = gen_storage_logs(0..100, num_batches);
    extend_db_state(&mut storage, logs).await;
}

pub(super) async fn extend_db_state(
    storage: &mut StorageProcessor<'_>,
    new_logs: impl IntoIterator<Item = Vec<StorageLog>>,
) {
    let next_l1_batch = storage.blocks_dal().get_sealed_l1_batch_number().await.0 + 1;

    let base_system_contracts = BaseSystemContracts::load_from_disk();
    for (idx, batch_logs) in (next_l1_batch..).zip(new_logs) {
        let batch_number = L1BatchNumber(idx);
        let mut header = L1BatchHeader::new(
            batch_number,
            0,
            Address::default(),
            base_system_contracts.hashes(),
        );
        header.is_finished = true;

        // Assumes that L1 batch consists of only one miniblock.
        let miniblock_number = MiniblockNumber(idx);
        let miniblock_header = MiniblockHeader {
            number: miniblock_number,
            timestamp: header.timestamp,
            hash: miniblock_hash(miniblock_number),
            l1_tx_count: header.l1_tx_count,
            l2_tx_count: header.l2_tx_count,
            base_fee_per_gas: header.base_fee_per_gas,
            l1_gas_price: 0,
            l2_fair_gas_price: 0,
            base_system_contracts_hashes: base_system_contracts.hashes(),
        };

        storage
            .blocks_dal()
            .insert_l1_batch(&header, BlockGasCount::default())
            .await;
        storage
            .blocks_dal()
            .insert_miniblock(&miniblock_header)
            .await;
        storage
            .storage_logs_dal()
            .insert_storage_logs(miniblock_number, &[(H256::zero(), batch_logs)])
            .await;
        storage
            .blocks_dal()
            .mark_miniblocks_as_executed_in_l1_batch(batch_number)
            .await;
        insert_initial_writes_for_batch(storage, batch_number).await;
    }
}

async fn insert_initial_writes_for_batch(
    connection: &mut StorageProcessor<'_>,
    l1_batch_number: L1BatchNumber,
) {
    let written_non_zero_slots: Vec<_> = connection
        .storage_logs_dal()
        .get_touched_slots_for_l1_batch(l1_batch_number)
        .await
        .into_iter()
        .filter_map(|(key, value)| (!value.is_zero()).then_some(key))
        .collect();
    let hashed_keys: Vec<_> = written_non_zero_slots
        .iter()
        .map(|key| key.hashed_key())
        .collect();
    let pre_written_slots = connection
        .storage_logs_dedup_dal()
        .filter_written_slots(&hashed_keys)
        .await;

    let keys_to_insert: Vec<_> = written_non_zero_slots
        .into_iter()
        .sorted()
        .filter(|key| !pre_written_slots.contains(&key.hashed_key()))
        .collect();
    connection
        .storage_logs_dedup_dal()
        .insert_initial_writes(l1_batch_number, &keys_to_insert)
        .await;
}

pub(super) fn gen_storage_logs(
    indices: ops::Range<u32>,
    num_batches: usize,
) -> Vec<Vec<StorageLog>> {
    // Addresses and keys of storage logs must be sorted for the `multi_block_workflow` test.
    let mut accounts = [
        "4b3af74f66ab1f0da3f2e4ec7a3cb99baf1af7b2",
        "ef4bb7b21c5fe7432a7d63876cc59ecc23b46636",
        "89b8988a018f5348f52eeac77155a793adf03ecc",
        "782806db027c08d36b2bed376b4271d1237626b3",
        "b2b57b76717ee02ae1327cc3cf1f40e76f692311",
    ]
    .map(|s| AccountTreeId::new(s.parse::<Address>().unwrap()));
    accounts.sort_unstable();

    let account_keys = (indices.start / 5)..(indices.end / 5);
    let proof_keys = accounts.iter().flat_map(|&account| {
        account_keys
            .clone()
            .map(move |i| StorageKey::new(account, u32_to_h256(i)))
    });
    let proof_values = indices.map(u32_to_h256);

    let logs: Vec<_> = proof_keys
        .zip(proof_values)
        .map(|(proof_key, proof_value)| StorageLog::new_write_log(proof_key, proof_value))
        .collect();
    for window in logs.windows(2) {
        let [prev, next] = window else { unreachable!() };
        assert!(prev.key < next.key);
    }

    logs.chunks(logs.len() / num_batches)
        .map(<[_]>::to_vec)
        .collect()
}

async fn remove_l1_batches(
    storage: &mut StorageProcessor<'_>,
    last_l1_batch_to_keep: L1BatchNumber,
) -> Vec<L1BatchHeader> {
    let sealed_l1_batch_number = storage.blocks_dal().get_sealed_l1_batch_number().await;
    assert!(sealed_l1_batch_number >= last_l1_batch_to_keep);

    let mut batch_headers = vec![];
    for batch_number in (last_l1_batch_to_keep.0 + 1)..=sealed_l1_batch_number.0 {
        let header = storage
            .blocks_dal()
            .get_l1_batch_header(L1BatchNumber(batch_number))
            .await;
        batch_headers.push(header.unwrap());
    }

    storage
        .blocks_dal()
        .delete_l1_batches(last_l1_batch_to_keep)
        .await;
    batch_headers
}

#[db_test]
async fn deduplication_works_as_expected(pool: ConnectionPool) {
    let mut storage = pool.access_storage().await;

    let first_validator = Address::repeat_byte(0x01);
    let base_system_contracts = BaseSystemContracts::load_from_disk();
    let system_contracts = get_system_smart_contracts();
    ensure_genesis_state(
        &mut storage,
        L2ChainId(270),
        &GenesisParams {
            first_validator,
            base_system_contracts,
            system_contracts,
        },
    )
    .await;

    let logs = gen_storage_logs(100..120, 1).pop().unwrap();
    let hashed_keys: Vec<_> = logs.iter().map(|log| log.key.hashed_key()).collect();
    extend_db_state(&mut storage, [logs.clone()]).await;

    let initial_writes = storage
        .storage_logs_dal()
        .get_l1_batches_for_initial_writes(&hashed_keys)
        .await;
    assert_eq!(initial_writes.len(), hashed_keys.len());
    assert!(initial_writes
        .values()
        .all(|&batch| batch == L1BatchNumber(1)));

    let mut new_logs = gen_storage_logs(120..140, 1).pop().unwrap();
    let new_hashed_keys: Vec<_> = new_logs.iter().map(|log| log.key.hashed_key()).collect();
    let updated_logs = logs.into_iter().step_by(2).map(|mut log| {
        log.value = H256::zero();
        log
    });
    new_logs.extend(updated_logs);
    extend_db_state(&mut storage, [new_logs]).await;

    // Initial writes for previously inserted keys should not change.
    let initial_writes = storage
        .storage_logs_dal()
        .get_l1_batches_for_initial_writes(&hashed_keys)
        .await;
    assert_eq!(initial_writes.len(), hashed_keys.len());
    assert!(initial_writes
        .values()
        .all(|&batch| batch == L1BatchNumber(1)));

    let initial_writes = storage
        .storage_logs_dal()
        .get_l1_batches_for_initial_writes(&new_hashed_keys)
        .await;
    assert_eq!(initial_writes.len(), new_hashed_keys.len());
    assert!(initial_writes
        .values()
        .all(|&batch| batch == L1BatchNumber(2)));

    let mut no_op_logs = gen_storage_logs(140..160, 1).pop().unwrap();
    let no_op_hashed_keys: Vec<_> = no_op_logs.iter().map(|log| log.key.hashed_key()).collect();
    for log in &mut no_op_logs {
        log.value = H256::zero();
    }
    extend_db_state(&mut storage, [no_op_logs.clone()]).await;

    let initial_writes = storage
        .storage_logs_dal()
        .get_l1_batches_for_initial_writes(&no_op_hashed_keys)
        .await;
    assert!(initial_writes.is_empty());

    let updated_logs: Vec<_> = no_op_logs
        .iter()
        .step_by(2)
        .map(|log| StorageLog {
            value: H256::repeat_byte(0x11),
            ..*log
        })
        .collect();
    no_op_logs.extend_from_slice(&updated_logs);
    extend_db_state(&mut storage, [no_op_logs]).await;

    let initial_writes = storage
        .storage_logs_dal()
        .get_l1_batches_for_initial_writes(&no_op_hashed_keys)
        .await;
    assert_eq!(initial_writes.len(), no_op_hashed_keys.len() / 2);
    for key in no_op_hashed_keys.iter().step_by(2) {
        assert_eq!(initial_writes[key], L1BatchNumber(4));
    }
}