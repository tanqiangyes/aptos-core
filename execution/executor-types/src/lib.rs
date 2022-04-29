// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

mod error;
mod executed_chunk;

pub use error::Error;

use anyhow::Result;
use aptos_crypto::{
    ed25519::Ed25519Signature,
    hash::{
        EventAccumulatorHasher, TransactionAccumulatorHasher, ACCUMULATOR_PLACEHOLDER_HASH,
        SPARSE_MERKLE_PLACEHOLDER_HASH,
    },
    HashValue,
};
use aptos_state_view::StateViewId;
use aptos_types::{
    contract_event::ContractEvent,
    epoch_state::EpochState,
    ledger_info::LedgerInfoWithSignatures,
    nibble::nibble_path::NibblePath,
    proof::{accumulator::InMemoryAccumulator, AccumulatorExtensionProof},
    state_store::{state_key::StateKey, state_value::StateValue},
    transaction::{
        Transaction, TransactionInfo, TransactionListWithProof, TransactionOutputListWithProof,
        TransactionStatus, Version,
    },
    write_set::WriteSet,
};
use scratchpad::ProofRead;
use serde::{Deserialize, Serialize};
use std::{cmp::max, collections::HashMap, sync::Arc};
use storage_interface::DbReader;

pub use executed_chunk::ExecutedChunk;
use storage_interface::verified_state_view::VerifiedStateView;

type SparseMerkleProof = aptos_types::proof::SparseMerkleProof<StateValue>;
type SparseMerkleTree = scratchpad::SparseMerkleTree<StateValue>;

pub trait ChunkExecutorTrait: Send + Sync {
    /// Verifies the transactions based on the provided proofs and ledger info. If the transactions
    /// are valid, executes them and returns the executed result for commit.
    /// 根据提供的证明和分类帐信息验证交易。如果事务有效，则执行它们并返回执行结果以进行提交。
    fn execute_chunk(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        // Target LI that has been verified independently: the proofs are relative to this version.
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<()>;

    /// Similar to `execute_chunk`, but instead of executing transactions, apply the transaction
    /// outputs directly to get the executed result.
    /// 类似于`execute_chunk`，但不是执行事务，而是直接应用事务输出来获取执行结果。
    fn apply_chunk(
        &self,
        txn_output_list_with_proof: TransactionOutputListWithProof,
        // Target LI that has been verified independently: the proofs are relative to this version.
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> anyhow::Result<()>;

    /// Commit a previously executed chunk. Returns a vector of reconfiguration
    /// events in the chunk and the transactions that were committed.
    /// 提交之前执行的块。返回块中的重新配置事件向量和已提交的事务。
    fn commit_chunk(&self) -> Result<(Vec<ContractEvent>, Vec<Transaction>)>;

    fn execute_and_commit_chunk(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<(Vec<ContractEvent>, Vec<Transaction>)>;

    fn apply_and_commit_chunk(
        &self,
        txn_output_list_with_proof: TransactionOutputListWithProof,
        verified_target_li: &LedgerInfoWithSignatures,
        epoch_change_li: Option<&LedgerInfoWithSignatures>,
    ) -> Result<(Vec<ContractEvent>, Vec<Transaction>)>;

    /// Resets the chunk executor by synchronizing state with storage.
    /// 通过将状态与存储同步来重置块执行器。
    fn reset(&self) -> Result<()>;
}

pub trait BlockExecutorTrait: Send + Sync {
    /// Get the latest committed block id
    fn committed_block_id(&self) -> HashValue;

    /// Reset the internal state including cache with newly fetched latest committed block from storage.
    fn reset(&self) -> Result<(), Error>;

    /// Executes a block.
    fn execute_block(
        &self,
        block: (HashValue, Vec<Transaction>),
        parent_block_id: HashValue,
    ) -> Result<StateComputeResult, Error>;

    /// Saves eligible blocks to persistent storage.
    /// If we have multiple blocks and not all of them have signatures, we may send them to storage
    /// in a few batches. For example, if we have
    /// ```text
    /// A <- B <- C <- D <- E
    /// ```
    /// and only `C` and `E` have signatures, we will send `A`, `B` and `C` in the first batch,
    /// then `D` and `E` later in the another batch.
    /// Commits a block and all its ancestors in a batch manner.
    /// 将符合条件的块保存到持久存储。如果我们有多个区块并且并非所有区块都有签名，我们可能会分几批将它们发送到存储中。例如，如果我们有
    /// ```text
    ///  A <- B <- C <- D <- E
    ///  ```
    /// 只有`C`和`E`有签名，我们将在第一批发送`A`、`B`和`C`，然后在另一批发送`D`和`E`。以批处理方式提交一个块及其所有祖先。
    fn commit_blocks(
        &self,
        block_ids: Vec<HashValue>,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
    ) -> Result<(), Error>;
}

pub trait TransactionReplayer: Send {
    fn replay(
        &self,
        transactions: Vec<Transaction>,
        transaction_infos: Vec<TransactionInfo>,
    ) -> Result<()>;

    fn commit(&self) -> Result<Arc<ExecutedChunk>>;
}

/// A structure that summarizes the result of the execution needed for consensus to agree on.
/// The execution is responsible for generating the ID of the new state, which is returned in the
/// result.
/// 一种结构，总结了达成共识所需的执行结果。执行负责生成新状态的 ID，并在结果中返回。
/// Not every transaction in the payload succeeds: the returned vector keeps the boolean status
/// of success / failure of the transactions.
/// Note that the specific details of compute_status are opaque to StateMachineReplication,
/// which is going to simply pass the results between StateComputer and TxnManager.
/// 并非负载中的每个事务都成功：返回的向量保持事务成功失败的布尔状态。
/// 注意，compute_status 的具体细节对 StateMachineReplication 是不透明的，StateMachineReplication 将简单地在 StateComputer 和 TxnManager 之间传递结果。
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct StateComputeResult {
    /// transaction accumulator root hash is identified as `state_id` in Consensus.
    /// 交易累加器根哈希在共识中被标识为 `state_id`。
    root_hash: HashValue,
    /// Represents the roots of all the full subtrees from left to right in this accumulator
    /// after the execution. For details, please see [`InMemoryAccumulator`](accumulator::InMemoryAccumulator).
    frozen_subtree_roots: Vec<HashValue>,

    /// The frozen subtrees roots of the parent block,
    /// 父块的冻结子树根，
    parent_frozen_subtree_roots: Vec<HashValue>,

    /// The number of leaves of the transaction accumulator after executing a proposed block.
    /// This state must be persisted to ensure that on restart that the version is calculated correctly.
    /// 执行提议的块后事务累加器的叶子数。必须保持此状态以确保在重新启动时正确计算版本。
    num_leaves: u64,

    /// The number of leaves after executing the parent block,
    /// 执行父块后的叶子数，
    parent_num_leaves: u64,

    /// If set, this is the new epoch info that should be changed to if this block is committed.
    /// 新epoch的信息，如果设置了，则应该在区块commited的时候改变
    epoch_state: Option<EpochState>,
    /// The compute status (success/failure) of the given payload. The specific details are opaque
    /// for StateMachineReplication, which is merely passing it between StateComputer and
    /// TxnManager.
    /// 给定负载的计算状态（成功失败）。 StateMachineReplication 的具体细节是不透明的，它只是在 StateComputer 和 TxnManager 之间传递它。
    compute_status: Vec<TransactionStatus>,

    /// The transaction info hashes of all success txns.
    /// 所有成功 txns 的交易信息哈希。
    transaction_info_hashes: Vec<HashValue>,

    /// The signature of the VoteProposal corresponding to this block.
    /// 该区块对应的 VoteProposal 的签名。
    signature: Option<Ed25519Signature>,

    reconfig_events: Vec<ContractEvent>,
}

impl StateComputeResult {
    pub fn new(
        root_hash: HashValue,
        frozen_subtree_roots: Vec<HashValue>,
        num_leaves: u64,
        parent_frozen_subtree_roots: Vec<HashValue>,
        parent_num_leaves: u64,
        epoch_state: Option<EpochState>,
        compute_status: Vec<TransactionStatus>,
        transaction_info_hashes: Vec<HashValue>,
        reconfig_events: Vec<ContractEvent>,
    ) -> Self {
        Self {
            root_hash,
            frozen_subtree_roots,
            num_leaves,
            parent_frozen_subtree_roots,
            parent_num_leaves,
            epoch_state,
            compute_status,
            transaction_info_hashes,
            reconfig_events,
            signature: None,
        }
    }

    /// generate a new dummy state compute result with a given root hash.
    /// this function is used in RandomComputeResultStateComputer to assert that the compute
    /// function is really called.
    /// 使用给定的根哈希生成新的虚拟状态计算结果。此函数在 RandomComputeResultStateComputer 中用于断言计算函数确实被调用。
    pub fn new_dummy_with_root_hash(root_hash: HashValue) -> Self {
        Self {
            root_hash,
            frozen_subtree_roots: vec![],
            num_leaves: 0,
            parent_frozen_subtree_roots: vec![],
            parent_num_leaves: 0,
            epoch_state: None,
            compute_status: vec![],
            transaction_info_hashes: vec![],
            reconfig_events: vec![],
            signature: None,
        }
    }

    /// generate a new dummy state compute result with ACCUMULATOR_PLACEHOLDER_HASH as the root hash.
    /// this function is used in ordering_state_computer as a dummy state compute result,
    /// where the real compute result is generated after ordering_state_computer.commit pushes
    /// the blocks and the finality proof to the execution phase.
    pub fn new_dummy() -> Self {
        StateComputeResult::new_dummy_with_root_hash(*ACCUMULATOR_PLACEHOLDER_HASH)
    }
}

impl StateComputeResult {
    pub fn version(&self) -> Version {
        max(self.num_leaves, 1)
            .checked_sub(1)
            .expect("Integer overflow occurred")
    }

    pub fn root_hash(&self) -> HashValue {
        self.root_hash
    }

    pub fn compute_status(&self) -> &Vec<TransactionStatus> {
        &self.compute_status
    }

    pub fn epoch_state(&self) -> &Option<EpochState> {
        &self.epoch_state
    }

    pub fn extension_proof(&self) -> AccumulatorExtensionProof<TransactionAccumulatorHasher> {
        AccumulatorExtensionProof::<TransactionAccumulatorHasher>::new(
            self.parent_frozen_subtree_roots.clone(),
            self.parent_num_leaves(),
            self.transaction_info_hashes().clone(),
        )
    }

    pub fn transaction_info_hashes(&self) -> &Vec<HashValue> {
        &self.transaction_info_hashes
    }

    pub fn num_leaves(&self) -> u64 {
        self.num_leaves
    }

    pub fn frozen_subtree_roots(&self) -> &Vec<HashValue> {
        &self.frozen_subtree_roots
    }

    pub fn parent_num_leaves(&self) -> u64 {
        self.parent_num_leaves
    }

    pub fn parent_frozen_subtree_roots(&self) -> &Vec<HashValue> {
        &self.parent_frozen_subtree_roots
    }

    pub fn has_reconfiguration(&self) -> bool {
        self.epoch_state.is_some()
    }

    pub fn reconfig_events(&self) -> &[ContractEvent] {
        &self.reconfig_events
    }

    pub fn signature(&self) -> &Option<Ed25519Signature> {
        &self.signature
    }

    pub fn set_signature(&mut self, sig: Ed25519Signature) {
        self.signature = Some(sig);
    }
}

/// A wrapper of the in-memory state sparse merkle tree and the transaction accumulator that
/// represent a specific state collectively. Usually it is a state after executing a block.
/// 内存中状态稀疏默克尔树和事务累加器的包装器，它们共同表示特定状态。通常是执行块后的状态。
#[derive(Clone, Debug)]
pub struct ExecutedTrees {
    /// The in-memory Sparse Merkle Tree representing a specific state after execution. If this
    /// tree is presenting the latest commited state, it will have a single Subtree node (or
    /// Empty node) whose hash equals the root hash of the newest Sparse Merkle Tree in
    /// storage.
    /// 表示执行后特定状态的内存中稀疏默克尔树。如果这棵树呈现最新的提交状态，它将有一个子树节点（或空节点），其哈希等于存储中最新的稀疏默克尔树的根哈希。
    state_tree: SparseMerkleTree,

    /// The in-memory Merkle Accumulator representing a blockchain state consistent with the
    /// `state_tree`.
    /// 内存中的 Merkle Accumulator 表示与“state_tree”一致的区块链状态。
    transaction_accumulator: Arc<InMemoryAccumulator<TransactionAccumulatorHasher>>,
}

impl ExecutedTrees {
    pub fn new_copy(
        state_tree: SparseMerkleTree,
        transaction_accumulator: Arc<InMemoryAccumulator<TransactionAccumulatorHasher>>,
    ) -> Self {
        Self {
            state_tree,
            transaction_accumulator,
        }
    }

    pub fn state_tree(&self) -> &SparseMerkleTree {
        &self.state_tree
    }

    pub fn txn_accumulator(&self) -> &Arc<InMemoryAccumulator<TransactionAccumulatorHasher>> {
        &self.transaction_accumulator
    }

    pub fn version(&self) -> Option<Version> {
        let num_elements = self.txn_accumulator().num_leaves() as u64;
        num_elements.checked_sub(1)
    }

    pub fn state_id(&self) -> HashValue {
        self.txn_accumulator().root_hash()
    }

    pub fn state_root(&self) -> HashValue {
        self.state_tree().root_hash()
    }

    pub fn new(
        state_root_hash: HashValue,
        frozen_subtrees_in_accumulator: Vec<HashValue>,
        num_leaves_in_accumulator: u64,
    ) -> ExecutedTrees {
        ExecutedTrees {
            state_tree: SparseMerkleTree::new(state_root_hash),
            transaction_accumulator: Arc::new(
                InMemoryAccumulator::new(frozen_subtrees_in_accumulator, num_leaves_in_accumulator)
                    .expect("The startup info read from storage should be valid."),
            ),
        }
    }

    pub fn new_empty() -> ExecutedTrees {
        Self::new(*SPARSE_MERKLE_PLACEHOLDER_HASH, vec![], 0)
    }

    pub fn is_same_view(&self, rhs: &Self) -> bool {
        self.transaction_accumulator.root_hash() == rhs.transaction_accumulator.root_hash()
    }

    pub fn state_view(
        &self,
        persisted_view: &Self,
        id: StateViewId,
        reader: Arc<dyn DbReader>,
    ) -> VerifiedStateView {
        VerifiedStateView::new(
            id,
            reader.clone(),
            persisted_view.version(),
            persisted_view.state_tree.root_hash(),
            self.state_tree.clone(),
        )
    }
}

impl Default for ExecutedTrees {
    fn default() -> Self {
        Self::new_empty()
    }
}

pub struct ProofReader {
    proofs: HashMap<HashValue, SparseMerkleProof>,
}

impl ProofReader {
    pub fn new(proofs: HashMap<HashValue, SparseMerkleProof>) -> Self {
        ProofReader { proofs }
    }

    pub fn new_empty() -> Self {
        Self::new(HashMap::new())
    }
}

impl ProofRead<StateValue> for ProofReader {
    fn get_proof(&self, key: HashValue) -> Option<&SparseMerkleProof> {
        self.proofs.get(&key)
    }
}

/// The entire set of data associated with a transaction. In addition to the output generated by VM
/// which includes the write set and events, this also has the in-memory trees.
/// 与事务关联的整个数据集。除了由 VM 生成的输出（包括写入集和事件）之外，它还有内存中的树。
#[derive(Clone, Debug)]
pub struct TransactionData {
    /// Each entry in this map represents the new value of a store store object touched by this
    /// transaction.
    /// 此映射中的每个条目都表示此事务涉及的商店商店对象的新值。
    state_updates: HashMap<StateKey, StateValue>,

    /// Each entry in this map represents the the hash of a newly generated jellyfish node
    /// and its corresponding nibble path.
    /// 此映射中的每个条目表示新生成的水母节点的哈希值及其对应的半字节路径。
    jf_node_hashes: HashMap<NibblePath, HashValue>,

    /// The writeset generated from this transaction.
    /// 从此事务生成的写入集。
    write_set: WriteSet,

    /// The list of events emitted during this transaction.
    /// 此事务期间发出的事件列表。
    events: Vec<ContractEvent>,

    /// List of reconfiguration events emitted during this transaction.
    /// 此事务期间发出的重新配置事件列表。
    reconfig_events: Vec<ContractEvent>,

    /// The execution status set by the VM.
    /// VM 设置的执行状态。
    status: TransactionStatus,

    /// The in-memory Merkle Accumulator that has all events emitted by this transaction.
    /// 具有此事务发出的所有事件的内存中 Merkle 累加器。
    event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,

    /// The amount of gas used.
    /// gas使用量
    gas_used: u64,

    /// TransactionInfo
    /// 交易信息
    txn_info: TransactionInfo,

    /// TransactionInfo.hash()
    txn_info_hash: HashValue,
}

impl TransactionData {
    pub fn new(
        state_updates: HashMap<StateKey, StateValue>,
        jf_node_hashes: HashMap<NibblePath, HashValue>,
        write_set: WriteSet,
        events: Vec<ContractEvent>,
        reconfig_events: Vec<ContractEvent>,
        status: TransactionStatus,
        event_tree: Arc<InMemoryAccumulator<EventAccumulatorHasher>>,
        gas_used: u64,
        txn_info: TransactionInfo,
        txn_info_hash: HashValue,
    ) -> Self {
        TransactionData {
            state_updates,
            jf_node_hashes,
            write_set,
            events,
            reconfig_events,
            status,
            event_tree,
            gas_used,
            txn_info,
            txn_info_hash,
        }
    }

    pub fn state_updates(&self) -> &HashMap<StateKey, StateValue> {
        &self.state_updates
    }

    pub fn jf_node_hashes(&self) -> &HashMap<NibblePath, HashValue> {
        &self.jf_node_hashes
    }

    pub fn write_set(&self) -> &WriteSet {
        &self.write_set
    }

    pub fn events(&self) -> &[ContractEvent] {
        &self.events
    }

    pub fn status(&self) -> &TransactionStatus {
        &self.status
    }

    pub fn event_root_hash(&self) -> HashValue {
        self.event_tree.root_hash()
    }

    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }

    pub fn txn_info_hash(&self) -> HashValue {
        self.txn_info_hash
    }
}
