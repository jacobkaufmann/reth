use alloc::vec::Vec;
use alloy_eips::eip7685::Requests;
use reth_primitives::{Recovered, TransactionSigned};
use revm_database::BundleState;

/// A helper type for ethereum block inputs that consists of a block and the total difficulty and
/// the associated inclusion list (IL).
#[derive(Debug)]
pub struct BlockExecutionInput<'a, Block> {
    /// The block to execute.
    pub block: &'a Block,
    /// The inclusion list (IL) that the block must satisfy.
    pub il: Vec<Recovered<TransactionSigned>>,
}

impl<'a, Block> BlockExecutionInput<'a, Block> {
    /// Creates a new input.
    pub const fn new(block: &'a Block, il: Vec<Recovered<TransactionSigned>>) -> Self {
        Self { block, il }
    }
}

impl<'a, Block> From<&'a Block> for BlockExecutionInput<'a, Block> {
    fn from(block: &'a Block) -> Self {
        Self::new(block, vec![])
    }
}


/// The result of executing a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockExecutionResult<T> {
    /// All the receipts of the transactions in the block.
    pub receipts: Vec<T>,
    /// All the EIP-7685 requests in the block.
    pub requests: Requests,
    /// The total gas used by the block.
    pub gas_used: u64,
}

/// [`BlockExecutionResult`] combined with state.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    derive_more::AsRef,
    derive_more::AsMut,
    derive_more::Deref,
    derive_more::DerefMut,
)]
pub struct BlockExecutionOutput<T> {
    /// All the receipts of the transactions in the block.
    #[as_ref]
    #[as_mut]
    #[deref]
    #[deref_mut]
    pub result: BlockExecutionResult<T>,
    /// The changed state of the block after execution.
    pub state: BundleState,
}
