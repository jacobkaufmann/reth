//! A basic Ethereum payload builder implementation.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(clippy::useless_let_if_seq)]

use alloy_consensus::{Header, Transaction, Typed2718, EMPTY_OMMER_ROOT_HASH};
use alloy_eips::{
    eip4844::MAX_DATA_GAS_PER_BLOCK, eip6110, eip7685::Requests, eip7840::BlobParams,
    merge::BEACON_NONCE,
};
use alloy_primitives::U256;
use reth_basic_payload_builder::{
    commit_withdrawals, is_better_payload, BuildArguments, BuildOutcome, PayloadBuilder,
    PayloadConfig,
};
use reth_chainspec::{ChainSpec, ChainSpecProvider};
use reth_errors::{ProviderError, RethError};
use reth_evm::{env::EvmEnv, system_calls::SystemCaller, ConfigureEvm, ConfigureEvmEnv, Evm, NextBlockEnvAttributes};
use reth_evm_ethereum::{eip6110::parse_deposits_from_receipts, EthEvm, EthEvmConfig};
use reth_execution_types::ExecutionOutcome;
use reth_payload_builder::{EthBuiltPayload, EthPayloadBuilderAttributes};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives::{
    Block, BlockBody, EthereumHardforks, InvalidTransactionError, Receipt, TransactionSigned,
};
use reth_primitives_traits::{
    proofs::{self},
    Block as _, SignedTransaction,
};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    error::InvalidPoolTransactionError, noop::NoopTransactionPool, BestTransactions,
    BestTransactionsAttributes, PoolTransaction, TransactionPool, ValidPoolTransaction,
};
use revm::{
    db::{states::bundle_state::BundleRetention, State},
    primitives::{
        Address, BlockEnv, CfgEnvWithHandlerCfg, EVMError, EnvWithHandlerCfg, InvalidTransaction,
        ResultAndState, TxEnv,
    },
    Database, DatabaseCommit,
};
use std::sync::Arc;
use tracing::{debug, trace, warn};

mod config;
pub use config::*;

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Ethereum payload builder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthereumPayloadBuilder<EvmConfig = EthEvmConfig> {
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    /// Payload builder configuration.
    builder_config: EthereumBuilderConfig,
}

impl<EvmConfig> EthereumPayloadBuilder<EvmConfig> {
    /// `EthereumPayloadBuilder` constructor.
    pub const fn new(evm_config: EvmConfig, builder_config: EthereumBuilderConfig) -> Self {
        Self { evm_config, builder_config }
    }
}

impl<EvmConfig> EthereumPayloadBuilder<EvmConfig>
where
    EvmConfig: ConfigureEvm<Header = Header>,
{
    /// Returns the configured [`EvmEnv`] for the targeted payload
    /// (that has the `parent` as its parent).
    fn evm_env(
        &self,
        config: &PayloadConfig<EthPayloadBuilderAttributes>,
        parent: &Header,
    ) -> Result<EvmEnv<EvmConfig::Spec>, EvmConfig::Error> {
        let next_attributes = NextBlockEnvAttributes {
            timestamp: config.attributes.timestamp(),
            suggested_fee_recipient: config.attributes.suggested_fee_recipient(),
            prev_randao: config.attributes.prev_randao(),
            gas_limit: self.builder_config.gas_limit(parent.gas_limit),
        };
        self.evm_config.next_evm_env(parent, next_attributes)
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<EvmConfig, Pool, Client> PayloadBuilder<Pool, Client> for EthereumPayloadBuilder<EvmConfig>
where
    EvmConfig: ConfigureEvm<Header = Header, Transaction = TransactionSigned>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = ChainSpec>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = EthPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, EthPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        let evm_env = self
            .evm_env(&args.config, &args.config.parent_header)
            .map_err(PayloadBuilderError::other)?;

        let pool = args.pool.clone();
        default_ethereum_payload(
            self.evm_config.clone(),
            self.builder_config.clone(),
            args,
            evm_env,
            |attributes| pool.best_transactions_with_attributes(attributes),
        )
    }

    fn build_empty_payload(
        &self,
        client: &Client,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        let args = BuildArguments::new(
            client,
            // we use defaults here because for the empty payload we don't need to execute anything
            NoopTransactionPool::default(),
            Default::default(),
            config,
            Default::default(),
            None,
        );

        let evm_env = self
            .evm_env(&args.config, &args.config.parent_header)
            .map_err(PayloadBuilderError::other)?;

        let pool = args.pool.clone();

        // NOTE
        //
        // the payload may not be empty if there is an IL to apply to the payload. the call to
        // `apply_inclusion_list` is in the definition of `default_ethereum_payload`.
        default_ethereum_payload(
            self.evm_config.clone(),
            self.builder_config.clone(),
            args,
            evm_env,
            |attributes| pool.best_transactions_with_attributes(attributes),
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

/// Constructs an Ethereum transaction payload using the best transactions from the pool.
///
/// Given build arguments including an Ethereum client, transaction pool,
/// and configuration, this function creates a transaction payload. Returns
/// a result indicating success with the payload or an error in case of failure.
#[inline]
pub fn default_ethereum_payload<EvmConfig, Pool, Client, F>(
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    args: BuildArguments<Pool, Client, EthPayloadBuilderAttributes, EthBuiltPayload>,
    evm_env: EvmEnv<EvmConfig::Spec>,
    best_txs: F,
) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<Header = Header, Transaction = TransactionSigned>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = ChainSpec>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter<Pool>,
{
    let BuildArguments { client, pool, mut cached_reads, config, cancel, best_payload } = args;

    let chain_spec = client.chain_spec();
    let state_provider = client.state_by_block_hash(config.parent_header.hash())?;
    let state = StateProviderDatabase::new(state_provider);
    let mut db =
        State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();
    let PayloadConfig { parent_header, attributes } = config;

    debug!(target: "payload_builder", id=%attributes.id, parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");
    let mut cumulative_gas_used = 0;
    let mut sum_blob_gas_used = 0;
    let block_gas_limit: u64 = evm_env.block_env.gas_limit.to::<u64>();
    let base_fee = evm_env.block_env.basefee.to::<u64>();

    let mut executed_txs = Vec::new();

    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,
        evm_env.block_env.get_blob_gasprice().map(|gasprice| gasprice as u64),
    ));
    let mut total_fees = U256::ZERO;

    let block_number = evm_env.block_env.number.to::<u64>();
    let beneficiary = evm_env.block_env.coinbase;

    let mut system_caller = SystemCaller::new(evm_config.clone(), chain_spec.clone());

    // apply eip-4788 pre block contract call
    system_caller
        .pre_block_beacon_root_contract_call(&mut db, &evm_env, attributes.parent_beacon_block_root)
        .map_err(|err| {
            warn!(target: "payload_builder",
                parent_hash=%parent_header.hash(),
                %err,
                "failed to apply beacon root contract call for payload"
            );
            PayloadBuilderError::Internal(err.into())
        })?;

    // apply eip-2935 blockhashes update
    system_caller.pre_block_blockhashes_contract_call(
        &mut db,
        &evm_env,
        parent_header.hash(),
    )
    .map_err(|err| {
        warn!(target: "payload_builder", parent_hash=%parent_header.hash(), %err, "failed to update parent header blockhashes for payload");
        PayloadBuilderError::Internal(err.into())
    })?;

    let mut evm = evm_config.evm_with_env(&mut db, evm_env);

    let mut receipts = Vec::new();
    while let Some(pool_tx) = best_txs.next() {
        // ensure we still have capacity for this transaction
        if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
            // we can't fit this transaction into the block, so we need to mark it as invalid
            // which also removes all dependent transaction from the iterator before we can
            // continue
            best_txs.mark_invalid(
                &pool_tx,
                InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
            );
            continue
        }

        // check if the job was cancelled, if so we can exit early
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled)
        }

        // convert tx to a signed transaction
        let tx = pool_tx.to_consensus();

        // There's only limited amount of blob space available per block, so we need to check if
        // the EIP-4844 can still fit in the block
        if let Some(blob_tx) = tx.as_eip4844() {
            let tx_blob_gas = blob_tx.blob_gas();
            if sum_blob_gas_used + tx_blob_gas > MAX_DATA_GAS_PER_BLOCK {
                // we can't fit this _blob_ transaction into the block, so we mark it as
                // invalid, which removes its dependent transactions from
                // the iterator. This is similar to the gas limit condition
                // for regular transactions above.
                trace!(target: "payload_builder", tx=?tx.hash(), ?sum_blob_gas_used, ?tx_blob_gas, "skipping blob transaction because it would exceed the max data gas per block");
                best_txs.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(
                        tx_blob_gas,
                        MAX_DATA_GAS_PER_BLOCK,
                    ),
                );
                continue
            }
        }

        // Configure the environment for the tx.
        let tx_env = evm_config.tx_env(tx.tx(), tx.signer());

        let ResultAndState { result, state } = match evm.transact(tx_env) {
            Ok(res) => res,
            Err(err) => {
                match err {
                    EVMError::Transaction(err) => {
                        if matches!(err, InvalidTransaction::NonceTooLow { .. }) {
                            // if the nonce is too low, we can skip this transaction
                            trace!(target: "payload_builder", %err, ?tx, "skipping nonce too low transaction");
                        } else {
                            // if the transaction is invalid, we can skip it and all of its
                            // descendants
                            trace!(target: "payload_builder", %err, ?tx, "skipping invalid transaction and its descendants");
                            best_txs.mark_invalid(
                                &pool_tx,
                                InvalidPoolTransactionError::Consensus(
                                    InvalidTransactionError::TxTypeNotSupported,
                                ),
                            );
                        }

                        continue
                    }
                    err => {
                        // this is an error that we should treat as fatal for this attempt
                        return Err(PayloadBuilderError::EvmExecutionError(err))
                    }
                }
            }
        };

        // commit changes
        evm.db_mut().commit(state);

        // add to the total blob gas used if the transaction successfully executed
        if let Some(blob_tx) = tx.as_eip4844() {
            let tx_blob_gas = blob_tx.blob_gas();
            sum_blob_gas_used += tx_blob_gas;

            // if we've reached the max data gas per block, we can skip blob txs entirely
            if sum_blob_gas_used == MAX_DATA_GAS_PER_BLOCK {
                best_txs.skip_blobs();
            }
        }

        let gas_used = result.gas_used();

        // add gas used by the transaction to cumulative gas used, before creating the receipt
        cumulative_gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        #[allow(clippy::needless_update)] // side-effect of optimism fields
        receipts.push(Receipt {
            tx_type: tx.tx_type(),
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs().into_iter().collect(),
            ..Default::default()
        });

        // update add to total fees
        let miner_fee =
            tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);

        // append transaction to the block body
        executed_txs.push(tx.into_tx());
    }

    // apply IL
    //
    // NOTE
    //
    // we apply after all other transactions so that we can ensure that the payload is IL-compliant.
    // if we attempted to apply the IL at the beginning, and then applied some other transactions,
    // then we would need to go back through the IL and retry any transactions that could not be
    // included at the start but may now be valid due to state changes caused by non-IL
    // transactions.
    let base_fee = evm.block().basefee.to::<u64>();
    let block_gas_limit: u64 = evm.block().gas_limit.to::<u64>();

    let empty_il = vec![];
    let il = attributes.il.as_ref().unwrap_or(&empty_il);

    // the IL bitfield tracks whether we need to consider the IL transaction at the corresponding
    // index any longer.
    //
    // if the tx could not be decoded, then we mark it false.
    // if the tx cannot execute for some reason that cannot change, then we mark it false.
    // if the tx executes successfully and is added to the block, then we mark it false.
    //
    // if a transaction from the IL is executed successfully, then we need to go back over each of
    // the remaining IL transactions that might now be valid.
    let mut il_bitfield: Vec<_> = il.iter().map(|tx| tx.is_some()).collect();

    let mut i = 0;
    let n = il.len();

    while i < n {
        if !il_bitfield[i] {
            i += 1;
            continue;
        }

        // if the IL tx were not able to be decoded, then the corresponding index in the bitfield
        // should be `false` in the check above.
        let tx = il[i].as_ref().expect("IL tx exists b/c it was decoded");

        // transaction is a blob transaction which is not supported
        //
        // NOTE
        //
        // we should catch this earlier, so that such a transaction does not occupy memory.
        if tx.is_eip4844() {
            il_bitfield[i] = false;
            i += 1;
            continue;
        }

        // transaction gas limit too high
        if cumulative_gas_used + tx.gas_limit() > block_gas_limit {
            il_bitfield[i] = false;
            i += 1;
            continue;
        }

        // Configure the environment for the tx.
        let tx_env = evm_config.tx_env(tx.tx(), tx.signer());

        let ResultAndState { result, state } = match evm.transact(tx_env) {
            Ok(res) => res,
            Err(err) => match err {
                EVMError::Transaction(err) => {
                    match err {
                        // a transaction whose nonce is too high may become valid.
                        // a transaction whose sender lacks funds may become valid.
                        InvalidTransaction::NonceTooHigh { .. } |
                        InvalidTransaction::LackOfFundForMaxFee { .. } => {}
                        _other => {
                            il_bitfield[i] = false;
                        }
                    }

                    i += 1;
                    continue;
                }
                err => return Err(PayloadBuilderError::EvmExecutionError(err)),
            },
        };

        evm.db_mut().commit(state);

        let gas_used = result.gas_used();
        cumulative_gas_used += gas_used;

        #[allow(clippy::needless_update)]
        receipts.push(Some(Receipt {
            tx_type: tx.tx_type(),
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs().into_iter().map(Into::into).collect(),
            ..Default::default()
        }));

        let miner_fee =
            tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);

        executed_senders.push(tx.signer());
        executed_txs.push(tx.clone().into_tx());

        // NOTE
        //
        // if we are here, then the transaction executed successfully.
        //
        // instead of setting the index to zero, we could keep track of a flag that indicates
        // whether or not we should perform another pass of the IL.
        il_bitfield[i] = false;
        i = 0;
    }

    // check if we have a better block
    if !is_better_payload(best_payload.as_ref(), total_fees) {
        // Release db
        drop(evm);

        // can skip building the block
        return Ok(BuildOutcome::Aborted { fees: total_fees, cached_reads })
    }

    // calculate the requests and the requests root
    let requests = if chain_spec.is_prague_active_at_timestamp(attributes.timestamp) {
        let deposit_requests = parse_deposits_from_receipts(&chain_spec, receipts.iter())
            .map_err(|err| PayloadBuilderError::Internal(RethError::Execution(err.into())))?;

        let mut requests = Requests::default();

        if !deposit_requests.is_empty() {
            requests.push_request_with_type(eip6110::DEPOSIT_REQUEST_TYPE, deposit_requests);
        }

        requests.extend(
            system_caller
                .apply_post_execution_changes(&mut evm)
                .map_err(|err| PayloadBuilderError::Internal(err.into()))?,
        );

        Some(requests)
    } else {
        None
    };

    // Release db
    drop(evm);

    let withdrawals_root =
        commit_withdrawals(&mut db, &chain_spec, attributes.timestamp, &attributes.withdrawals)?;

    // merge all transitions into bundle state, this would apply the withdrawal balance changes
    // and 4788 contract call
    db.merge_transitions(BundleRetention::Reverts);

    let requests_hash = requests.as_ref().map(|requests| requests.requests_hash());
    let execution_outcome = ExecutionOutcome::new(
        db.take_bundle(),
        vec![receipts].into(),
        block_number,
        vec![requests.clone().unwrap_or_default()],
    );
    let receipts_root =
        execution_outcome.ethereum_receipts_root(block_number).expect("Number is in range");
    let logs_bloom = execution_outcome.block_logs_bloom(block_number).expect("Number is in range");

    // calculate the state root
    let hashed_state = db.database.db.hashed_post_state(execution_outcome.state());
    let (state_root, _) = {
        db.database.inner().state_root_with_updates(hashed_state).inspect_err(|err| {
            warn!(target: "payload_builder",
                parent_hash=%parent_header.hash(),
                %err,
                "failed to calculate state root for payload"
            );
        })?
    };

    // create the block header
    let transactions_root = proofs::calculate_transaction_root(&executed_txs);

    // initialize empty blob sidecars at first. If cancun is active then this will
    let mut blob_sidecars = Vec::new();
    let mut excess_blob_gas = None;
    let mut blob_gas_used = None;

    // only determine cancun fields when active
    if chain_spec.is_cancun_active_at_timestamp(attributes.timestamp) {
        // grab the blob sidecars from the executed txs
        blob_sidecars = pool
            .get_all_blobs_exact(
                executed_txs.iter().filter(|tx| tx.is_eip4844()).map(|tx| *tx.tx_hash()).collect(),
            )
            .map_err(PayloadBuilderError::other)?;

        excess_blob_gas = if chain_spec.is_cancun_active_at_timestamp(parent_header.timestamp) {
            let blob_params = if chain_spec.is_prague_active_at_timestamp(attributes.timestamp) {
                BlobParams::prague()
            } else {
                // cancun
                BlobParams::cancun()
            };
            parent_header.next_block_excess_blob_gas(blob_params)
        } else {
            // for the first post-fork block, both parent.blob_gas_used and
            // parent.excess_blob_gas are evaluated as 0
            Some(alloy_eips::eip4844::calc_excess_blob_gas(0, 0))
        };

        blob_gas_used = Some(sum_blob_gas_used);
    }

    let header = Header {
        parent_hash: parent_header.hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary,
        state_root,
        transactions_root,
        receipts_root,
        withdrawals_root,
        logs_bloom,
        timestamp: attributes.timestamp,
        mix_hash: attributes.prev_randao,
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: Some(base_fee),
        number: parent_header.number + 1,
        gas_limit: block_gas_limit,
        difficulty: U256::ZERO,
        gas_used: cumulative_gas_used,
        extra_data: builder_config.extra_data,
        parent_beacon_block_root: attributes.parent_beacon_block_root,
        blob_gas_used,
        excess_blob_gas,
        requests_hash,
    };

    let withdrawals = chain_spec
        .is_shanghai_active_at_timestamp(attributes.timestamp)
        .then(|| attributes.withdrawals.clone());

    // seal the block
    let block = Block {
        header,
        body: BlockBody { transactions: executed_txs, ommers: vec![], withdrawals },
    };

    let sealed_block = Arc::new(block.seal_slow());
    debug!(target: "payload_builder", id=%attributes.id, sealed_block_header = ?sealed_block.sealed_header(), "sealed built block");

    let mut payload = EthBuiltPayload::new(attributes.id, sealed_block, total_fees, requests);

    // extend the payload with the blob sidecars from the executed txs
    payload.extend_sidecars(blob_sidecars.into_iter().map(Arc::unwrap_or_clone));

    Ok(BuildOutcome::Better { payload, cached_reads })
}

