// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::error::{MempoolError, StateSyncError};
use anyhow::Result;
use aptos_crypto::HashValue;
use aptos_types::ledger_info::LedgerInfoWithSignatures;
use consensus_types::{block::Block, common::Payload, executed_block::ExecutedBlock};
use executor_types::{Error as ExecutionError, StateComputeResult};
use futures::future::BoxFuture;
use std::sync::Arc;

pub type StateComputerCommitCallBackType =
    Box<dyn FnOnce(&[Arc<ExecutedBlock>], LedgerInfoWithSignatures) + Send + Sync>;

/// Retrieves and updates the status of transactions on demand (e.g., via talking with Mempool)
/// 按需检索和更新交易状态（例如，通过与 Mempool 交谈）
#[async_trait::async_trait]
pub trait TxnManager: Send + Sync {
    /// Brings new transactions to be applied.
    /// The `exclude_txns` list includes the transactions that are already pending in the
    /// branch of blocks consensus is trying to extend.
    ///
    /// wait_callback is executed when there's no transactions available and it decides to wait.
    /// pending_ordering indicates if we should long poll mempool or propose empty blocks to help commit pending txns
    /// 带来要应用的新事务。 exclude_txns列表包括已在区块共识试图扩展的分支中挂起的交易。
    /// wait_callback 在没有可用事务并决定等待时执行。 pending_ordering 指示我们是否应该长轮询内存池或建议空块来帮助提交待处理的 txns
    async fn pull_txns(
        &self,
        max_size: u64,
        exclude: Vec<&Payload>,
        wait_callback: BoxFuture<'static, ()>,
        pending_ordering: bool,
    ) -> Result<Payload, MempoolError>;

    /// Notifies TxnManager about the txns which failed execution. (Committed txns is notified by
    /// state sync.)
    /// 通知交易执行失败
    async fn notify_failed_txn(
        &self,
        block: &Block,
        compute_result: &StateComputeResult,
    ) -> Result<(), MempoolError>;

    /// Helper to trace transactions after block is generated
    fn trace_transactions(&self, _block: &Block) {}
}

/// While Consensus is managing proposed blocks, `StateComputer` is managing the results of the
/// (speculative) execution of their payload.
/// StateComputer is using proposed block ids for identifying the transactions.
/// Consensus 管理提议的块时，“StateComputer”管理其有效负载的（推测）执行结果。 StateComputer 正在使用提议的区块 ID 来识别交易。
#[async_trait::async_trait]
pub trait StateComputer: Send + Sync {
    /// How to execute a sequence of transactions and obtain the next state. While some of the
    /// transactions succeed, some of them can fail.
    /// In case all the transactions are failed, new_state_id is equal to the previous state id.
    /// 如何执行一系列交易并获得下一个状态。虽然一些事务成功，但其中一些可能会失败。
    /// 如果所有事务都失败，new_state_id 等于之前的状态 id。
    async fn compute(
        &self,
        // The block that will be computed.
        block: &Block,
        // The parent block root hash.
        parent_block_id: HashValue,
    ) -> Result<StateComputeResult, ExecutionError>;

    /// Send a successful commit. A future is fulfilled when the state is finalized.
    /// 发送成功的提交。当状态最终确定时，未来就实现了
    async fn commit(
        &self,
        blocks: &[Arc<ExecutedBlock>],
        finality_proof: LedgerInfoWithSignatures,
        callback: StateComputerCommitCallBackType,
    ) -> Result<(), ExecutionError>;

    /// Best effort state synchronization to the given target LedgerInfo.
    /// In case of success (`Result::Ok`) the LI of storage is at the given target.
    /// In case of failure (`Result::Error`) the LI of storage remains unchanged, and the validator
    /// can assume there were no modifications to the storage made.
    /// 与给定目标 LedgerInfo 的尽力而为状态同步。如果成功（ Result::Ok ），存储的 LI 位于给定目标。在失败的情况下（ Result::Error ），存储的 LI 保持不变，验证者可以假设没有对存储进行任何修改
    async fn sync_to(&self, target: LedgerInfoWithSignatures) -> Result<(), StateSyncError>;
}
