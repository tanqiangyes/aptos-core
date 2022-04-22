// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{error::MempoolError, state_replication::TxnManager};
use anyhow::{format_err, Result};
use aptos_logger::prelude::*;
use aptos_mempool::{ConsensusRequest, ConsensusResponse, TransactionSummary};
use aptos_metrics::monitor;
use aptos_types::transaction::TransactionStatus;
use consensus_types::{block::Block, common::Payload};
use executor_types::StateComputeResult;
use fail::fail_point;
use futures::{
    channel::{mpsc, oneshot},
    future::BoxFuture,
};
use itertools::Itertools;
use std::time::Duration;
use tokio::time::{sleep, timeout};

const NO_TXN_DELAY: u64 = 30;

/// Proxy interface to mempool
/// 到内存池的代理接口
#[derive(Clone)]
pub struct MempoolProxy {
    consensus_to_mempool_sender: mpsc::Sender<ConsensusRequest>,
    poll_count: u64,
    /// Timeout for consensus to get an ack from mempool for executed transactions (in milliseconds)
    /// 从内存池中获取执行交易确认的共识超时（以毫秒为单位）
    mempool_executed_txn_timeout_ms: u64,
    /// Timeout for consensus to pull transactions from mempool and get a response (in milliseconds)
    /// 从内存池中提取交易并获得响应的共识超时（以毫秒为单位）
    mempool_txn_pull_timeout_ms: u64,
}

impl MempoolProxy {
    pub fn new(
        consensus_to_mempool_sender: mpsc::Sender<ConsensusRequest>,
        poll_count: u64,
        mempool_txn_pull_timeout_ms: u64,
        mempool_executed_txn_timeout_ms: u64,
    ) -> Self {
        assert!(
            poll_count > 0,
            "poll_count = 0 won't pull any txns from mempool"
        );
        Self {
            consensus_to_mempool_sender,
            poll_count,
            mempool_executed_txn_timeout_ms,
            mempool_txn_pull_timeout_ms,
        }
    }

    async fn pull_internal(
        &self,
        max_size: u64,
        exclude_txns: Vec<TransactionSummary>,
    ) -> Result<Payload, MempoolError> {
        let (callback, callback_rcv) = oneshot::channel();
        let req = ConsensusRequest::GetBlockRequest(max_size, exclude_txns.clone(), callback);
        // send to shared mempool
        self.consensus_to_mempool_sender//发送信息
            .clone()
            .try_send(req)
            .map_err(anyhow::Error::from)?;
        // wait for response
        match monitor!(//等待返回
            "pull_txn",
            timeout(
                Duration::from_millis(self.mempool_txn_pull_timeout_ms),
                callback_rcv
            )
            .await
        ) {
            Err(_) => {
                Err(anyhow::anyhow!("[consensus] did not receive GetBlockResponse on time").into())
            }
            Ok(resp) => match resp.map_err(anyhow::Error::from)?? {
                ConsensusResponse::GetBlockResponse(txns) => Ok(txns),
                _ => Err(
                    anyhow::anyhow!("[consensus] did not receive expected GetBlockResponse").into(),
                ),
            },
        }
    }
}

#[async_trait::async_trait]
impl TxnManager for MempoolProxy {
    async fn pull_txns(
        &self,
        max_size: u64,
        exclude_payloads: Vec<&Payload>,
        wait_callback: BoxFuture<'static, ()>,
        pending_ordering: bool,
    ) -> Result<Payload, MempoolError> {
        fail_point!("consensus::pull_txns", |_| {
            Err(anyhow::anyhow!("Injected error in pull_txns").into())
        });
        let mut exclude_txns = vec![];
        for payload in exclude_payloads {//循环取出交易发送者和交易号
            for transaction in payload {
                exclude_txns.push(TransactionSummary {
                    sender: transaction.sender(),
                    sequence_number: transaction.sequence_number(),
                });
            }
        }
        let mut callback_wrapper = Some(wait_callback);
        // keep polling mempool until there's txn available or there's still pending txns
        // 继续轮询内存池，直到有可用的 txn 或仍有待处理的 txn
        let mut count = self.poll_count;
        let txns = loop {
            count -= 1;
            let txns = self.pull_internal(max_size, exclude_txns.clone()).await?;
            if txns.is_empty() && !pending_ordering && count > 0 {
                if let Some(callback) = callback_wrapper.take() {
                    callback.await;
                }
                sleep(Duration::from_millis(NO_TXN_DELAY)).await;
                continue;
            }
            break txns;
        };
        debug!(
            poll_count = self.poll_count - count,
            "Pull txn from mempool"
        );
        Ok(txns)
    }

    async fn notify_failed_txn(
        &self,
        block: &Block,
        compute_results: &StateComputeResult,
    ) -> Result<(), MempoolError> {
        let mut rejected_txns = vec![];
        let txns = match block.payload() {//取出块中的交易
            Some(txns) => txns,
            None => return Ok(()),
        };
        // skip the block metadata txn result
        for (txn, status) in txns
            .iter()
            .zip_eq(compute_results.compute_status().iter().skip(1))
        {
            if let TransactionStatus::Discard(_) = status {
                rejected_txns.push(TransactionSummary {
                    sender: txn.sender(),
                    sequence_number: txn.sequence_number(),
                });
            }
        }

        if rejected_txns.is_empty() {
            return Ok(());
        }

        let (callback, callback_rcv) = oneshot::channel();
        let req = ConsensusRequest::RejectNotification(rejected_txns, callback);

        // send to shared mempool
        self.consensus_to_mempool_sender//通知这些交易被拒绝
            .clone()
            .try_send(req)
            .map_err(anyhow::Error::from)?;

        if let Err(e) = monitor!(//等待回信
            "notify_mempool",
            timeout(
                Duration::from_millis(self.mempool_executed_txn_timeout_ms),
                callback_rcv
            )
            .await
        ) {
            Err(format_err!("[consensus] txn manager did not receive ACK for commit notification sent to mempool on time: {:?}", e).into())
        } else {
            Ok(())
        }
    }
}
