// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    block_storage::BlockReader, state_replication::TxnManager, util::time_service::TimeService,
};
use anyhow::{bail, ensure, format_err, Context};
use consensus_types::{
    block::Block,
    block_data::BlockData,
    common::{Author, Round},
    quorum_cert::QuorumCert,
};

use aptos_infallible::Mutex;
use futures::future::BoxFuture;
use std::sync::Arc;

#[cfg(test)]
#[path = "proposal_generator_test.rs"]
mod proposal_generator_test;

/// ProposalGenerator is responsible for generating the proposed block on demand: it's typically
/// used by a validator that believes it's a valid candidate for serving as a proposer at a given
/// round.
/// ProposalGenerator is the one choosing the branch to extend:
/// - round is given by the caller (typically determined by RoundState).
/// The transactions for the proposed block are delivered by TxnManager.
///
/// TxnManager should be aware of the pending transactions in the branch that it is extending,
/// such that it will filter them out to avoid transaction duplication.
/// ProposalGenerator 负责按需生成提议的块：它通常由认为它是在给定轮次中充当提议者的有效候选人的验证者使用。 ProposalGenerator 是选择要扩展的分支：
/// round 由调用者给出（通常由 RoundState 确定）。提议区块的交易由 TxnManager 交付。
/// TxnManager 应该知道它正在扩展的分支中的待处理事务，以便将它们过滤掉以避免事务重复。
pub struct ProposalGenerator {
    // The account address of this validator
    author: Author,
    // Block store is queried both for finding the branch to extend and for generating the
    // proposed block.
    block_store: Arc<dyn BlockReader + Send + Sync>,
    // Transaction manager is delivering the transactions.
    txn_manager: Arc<dyn TxnManager>,
    // Time service to generate block timestamps
    time_service: Arc<dyn TimeService>,
    // Max number of transactions to be added to a proposed block.
    max_block_size: u64,
    // Last round that a proposal was generated
    last_round_generated: Mutex<Round>,
}

impl ProposalGenerator {
    pub fn new(
        author: Author,
        block_store: Arc<dyn BlockReader + Send + Sync>,
        txn_manager: Arc<dyn TxnManager>,
        time_service: Arc<dyn TimeService>,
        max_block_size: u64,
    ) -> Self {
        Self {
            author,
            block_store,
            txn_manager,
            time_service,
            max_block_size,
            last_round_generated: Mutex::new(0),
        }
    }

    pub fn author(&self) -> Author {
        self.author
    }

    /// Creates a NIL block proposal extending the highest certified block from the block store.
    pub fn generate_nil_block(&self, round: Round) -> anyhow::Result<Block> {
        let hqc = self.ensure_highest_quorum_cert(round)?;
        Ok(Block::new_nil(round, hqc.as_ref().clone()))
    }

    /// The function generates a new proposal block: the returned future is fulfilled when the
    /// payload is delivered by the TxnManager implementation.  At most one proposal can be
    /// generated per round (no proposal equivocation allowed).
    /// Errors returned by the TxnManager implementation are propagated to the caller.
    /// The logic for choosing the branch to extend is as follows:
    /// 1. The function gets the highest head of a one-chain from block tree.
    /// The new proposal must extend hqc to ensure optimistic responsiveness.
    /// 2. The round is provided by the caller.
    /// 3. In case a given round is not greater than the calculated parent, return an OldRound
    /// error.
    /// 该函数生成一个新的提议块：当 TxnManager 实现传递有效负载时，返回的未来得到满足。每轮最多可以生成一个提案（不允许提案模棱两可）。
    /// TxnManager 实现返回的错误会传播给调用者。选择要扩展的分支的逻辑如下：
    /// 1、该函数从块树中获取单链的最高头部。新提案必须扩展 hqc 以确保乐观的响应能力。
    /// 2、该回合由调用者提供。
    /// 3、如果给定轮数不大于计算的父轮，则返回 OldRound 错误。
    pub async fn generate_proposal(
        &mut self,
        round: Round,
        wait_callback: BoxFuture<'static, ()>,
    ) -> anyhow::Result<BlockData> {
        {
            let mut last_round_generated = self.last_round_generated.lock();
            if *last_round_generated < round {
                *last_round_generated = round;
            } else {
                bail!("Already proposed in the round {}", round);
            }
        }

        let hqc = self.ensure_highest_quorum_cert(round)?;

        let (payload, timestamp) = if hqc.certified_block().has_reconfiguration() {
            // Reconfiguration rule - we propose empty blocks with parents' timestamp
            // after reconfiguration until it's committed
            // 重新配置规则 - 我们在重新配置后建议带有父时间戳的空块，直到它被提交
            (vec![], hqc.certified_block().timestamp_usecs())
        } else {
            // One needs to hold the blocks with the references to the payloads while get_block is
            // being executed: pending blocks vector keeps all the pending ancestors of the extended branch.
            // 在执行 get_block 时，需要保存具有对有效负载的引用的块：挂起的块向量保留扩展分支的所有挂起的祖先。
            let mut pending_blocks = self
                .block_store
                .path_from_commit_root(hqc.certified_block().id())
                .ok_or_else(|| format_err!("HQC {} already pruned", hqc.certified_block().id()))?;
            // Avoid txn manager long poll if the root block has txns, so that the leader can
            // deliver the commit proof to others without delay.
            // 如果根块有 txns，则避免 txn manager 长轮询，以便领导者可以立即将提交证明传递给其他人。
            pending_blocks.push(self.block_store.commit_root());

            // Exclude all the pending transactions: these are all the ancestors of
            // parent (including) up to the root (including).
            // 排除所有未决交易：这些是父（包括）的所有祖先，直到根（包括）。
            let exclude_payload: Vec<&Vec<_>> = pending_blocks
                .iter()
                .flat_map(|block| block.payload())
                .collect();

            let pending_ordering = self
                .block_store
                .path_from_ordered_root(hqc.certified_block().id())
                .ok_or_else(|| format_err!("HQC {} already pruned", hqc.certified_block().id()))?
                .iter()
                .any(|block| !block.payload().map_or(true, |txns| txns.is_empty()));

            // All proposed blocks in a branch are guaranteed to have increasing timestamps
            // since their predecessor block will not be added to the BlockStore until
            // the local time exceeds it.
            // 分支中的所有提议块都保证具有增加的时间戳，因为在本地时间超过它之前，它们的前一个块不会被添加到 BlockStore。
            let timestamp = self.time_service.get_current_timestamp();

            let payload = self
                .txn_manager
                .pull_txns(
                    self.max_block_size,
                    exclude_payload,
                    wait_callback,
                    pending_ordering,
                )
                .await
                .context("Fail to retrieve txn")?;

            (payload, timestamp.as_micros() as u64)
        };

        // create block proposal
        Ok(BlockData::new_proposal(
            payload,
            self.author,
            round,
            timestamp,
            hqc.as_ref().clone(),
        ))
    }

    fn ensure_highest_quorum_cert(&self, round: Round) -> anyhow::Result<Arc<QuorumCert>> {
        let hqc = self.block_store.highest_quorum_cert();
        ensure!(
            hqc.certified_block().round() < round,
            "Given round {} is lower than hqc round {}",
            round,
            hqc.certified_block().round()
        );
        ensure!(
            !hqc.ends_epoch(),
            "The epoch has already ended,a proposal is not allowed to generated"
        );

        Ok(hqc)
    }
}
