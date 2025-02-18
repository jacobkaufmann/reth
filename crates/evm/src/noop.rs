//! A no operation block executor implementation.

use crate::{
    execute::{BlockExecutorProvider, Executor},
    system_calls::OnStateHook,
    Database,
};
use reth_execution_errors::BlockExecutionError;
use reth_execution_types::{BlockExecutionInput, BlockExecutionOutput, BlockExecutionResult, ExecutionOutcome};
use reth_primitives::{NodePrimitives, RecoveredBlock};

const UNAVAILABLE_FOR_NOOP: &str = "execution unavailable for noop";

/// A [`BlockExecutorProvider`] implementation that does nothing.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct NoopBlockExecutorProvider<P>(core::marker::PhantomData<P>);

impl<P: NodePrimitives> BlockExecutorProvider for NoopBlockExecutorProvider<P> {
    type Primitives = P;

    type Executor<DB: Database> = Self;

    fn executor<DB>(&self, _: DB) -> Self::Executor<DB>
    where
        DB: Database,
    {
        Self::default()
    }
}

impl<DB: Database, P: NodePrimitives> Executor<DB> for NoopBlockExecutorProvider<P> {
    type Primitives = P;
    type Error = BlockExecutionError;

    fn execute_one<'a>(
        &mut self,
        _block: BlockExecutionInput<'a, RecoveredBlock<<Self::Primitives as NodePrimitives>::Block>>,
    ) -> Result<BlockExecutionResult<<Self::Primitives as NodePrimitives>::Receipt>, Self::Error>
    {
        Err(BlockExecutionError::msg(UNAVAILABLE_FOR_NOOP))
    }

    fn execute_one_with_state_hook<'a, F>(
        &mut self,
        _block: BlockExecutionInput<'a, RecoveredBlock<<Self::Primitives as NodePrimitives>::Block>>,
        _state_hook: F,
    ) -> Result<BlockExecutionResult<<Self::Primitives as NodePrimitives>::Receipt>, Self::Error>
    where
        F: OnStateHook + 'static,
    {
        Err(BlockExecutionError::msg(UNAVAILABLE_FOR_NOOP))
    }

    fn into_state(self) -> revm_database::State<DB> {
        unreachable!()
    }

    fn size_hint(&self) -> usize {
        0
    }
}
