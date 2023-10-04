// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use aptos_aggregator::{delayed_change::DelayedChange, delta_change_set::DeltaOp};
use aptos_mvhashmap::types::TxnIndex;
use aptos_types::{
    fee_statement::FeeStatement, transaction::BlockExecutableTransaction as Transaction,
    write_set::WriteOp,
};
use aptos_vm_types::resolver::{TExecutorView, TResourceGroupView};
use move_core_types::{
    value::MoveTypeLayout,
    vm_status::{StatusCode, VMStatus},
};
use std::{collections::BTreeMap, fmt::Debug, sync::Arc};

/// The execution result of a transaction
#[derive(Debug)]
pub enum ExecutionStatus<O, E> {
    /// Transaction was executed successfully.
    Success(O),
    /// Transaction hit a none recoverable error during execution, halt the execution and propagate
    /// the error back to the caller.
    Abort(E),
    /// Transaction was executed successfully, but will skip the execution of the trailing
    /// transactions in the list
    SkipRest(O),
}

/// Inference result of a transaction.
pub struct Accesses<K> {
    pub keys_read: Vec<K>,
    pub keys_written: Vec<K>,
}

pub enum ErrorCategory {
    CodeInvariantError,
    SpeculativeExecutionError,
    ValidError,
}

pub trait CategorizeError {
    fn categorize(&self) -> ErrorCategory;
}

impl CategorizeError for usize {
    fn categorize(&self) -> ErrorCategory {
        ErrorCategory::ValidError
    }
}

impl CategorizeError for VMStatus {
    fn categorize(&self) -> ErrorCategory {
        match self.status_code() {
            StatusCode::DELAYED_FIELDS_CODE_INVARIANT_ERROR => ErrorCategory::CodeInvariantError,
            StatusCode::SPECULATIVE_EXECUTION_ABORT_ERROR => {
                ErrorCategory::SpeculativeExecutionError
            },
            _ => ErrorCategory::ValidError,
        }
    }
}

/// Trait for single threaded transaction executor.
// TODO: Sync should not be required. Sync is only introduced because this trait occurs as a phantom type of executor struct.
pub trait ExecutorTask: Sync {
    /// Type of transaction and its associated key and value.
    type Txn: Transaction;

    /// The output of a transaction. This should contain the side effect of this transaction.
    type Output: TransactionOutput<Txn = Self::Txn> + 'static;

    /// Type of error when the executor failed to process a transaction and needs to abort.
    type Error: Debug + Clone + Send + Sync + Eq + CategorizeError + 'static;

    /// Type to initialize the single thread transaction executor. Copy and Sync are required because
    /// we will create an instance of executor on each individual thread.
    type Argument: Sync + Copy;

    /// Create an instance of the transaction executor.
    fn init(args: Self::Argument) -> Self;

    /// Execute a single transaction given the view of the current state.
    fn execute_transaction(
        &self,
        view: &(impl TExecutorView<
            <Self::Txn as Transaction>::Key,
            <Self::Txn as Transaction>::Tag,
            MoveTypeLayout,
            <Self::Txn as Transaction>::Identifier,
        > + TResourceGroupView<
            GroupKey = <Self::Txn as Transaction>::Key,
            ResourceTag = <Self::Txn as Transaction>::Tag,
            Layout = MoveTypeLayout,
        >),
        txn: &Self::Txn,
        txn_idx: TxnIndex,
        materialize_deltas: bool,
    ) -> ExecutionStatus<Self::Output, Self::Error>;
}

/// Trait for execution result of a single transaction.
pub trait TransactionOutput: Send + Sync + Debug {
    /// Type of transaction and its associated key and value.
    type Txn: Transaction;

    /// Get the writes of a transaction from its output, separately for resources, modules and
    /// aggregator_v1.
    fn resource_write_set(
        &self,
    ) -> BTreeMap<
        <Self::Txn as Transaction>::Key,
        (
            <Self::Txn as Transaction>::Value,
            Option<Arc<MoveTypeLayout>>,
        ),
    >;

    fn module_write_set(
        &self,
    ) -> BTreeMap<<Self::Txn as Transaction>::Key, <Self::Txn as Transaction>::Value>;

    fn aggregator_v1_write_set(
        &self,
    ) -> BTreeMap<<Self::Txn as Transaction>::Key, <Self::Txn as Transaction>::Value>;

    /// Get the aggregator V1 deltas of a transaction from its output.
    fn aggregator_v1_delta_set(&self) -> BTreeMap<<Self::Txn as Transaction>::Key, DeltaOp>;

    /// Get the delayed field changes of a transaction from its output.
    fn delayed_field_change_set(
        &self,
    ) -> BTreeMap<
        <Self::Txn as Transaction>::Identifier,
        DelayedChange<<Self::Txn as Transaction>::Identifier>,
    >;

    /// Get the events of a transaction from its output.
    fn get_events(&self) -> Vec<(<Self::Txn as Transaction>::Event, Option<MoveTypeLayout>)>;

    /// Execution output for transactions that comes after SkipRest signal.
    fn skip_output() -> Self;

    /// In parallel execution, will be called once per transaction when the output is
    /// ready to be committed. In sequential execution, won't be called (deltas are
    /// materialized and incorporated during execution).
    fn incorporate_delta_writes(
        &self,
        delta_writes: Vec<(<Self::Txn as Transaction>::Key, WriteOp)>,
    );

    /// In parallel execution, will be called once per transaction when the output is
    /// ready to be committed. In sequential execution, won't be called (deltas are
    /// materialized and incorporated during execution).
    fn incorporate_materialized_txn_output(
        &self,
        aggregator_v1_writes: Vec<(<Self::Txn as Transaction>::Key, WriteOp)>,
        patched_resource_write_set: BTreeMap<
            <Self::Txn as Transaction>::Key,
            <Self::Txn as Transaction>::Value,
        >,
        patched_events: Vec<<Self::Txn as Transaction>::Event>,
    );

    /// Return the fee statement of the transaction.
    fn fee_statement(&self) -> FeeStatement;
}
