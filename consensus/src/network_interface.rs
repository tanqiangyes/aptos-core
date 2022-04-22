// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

//! Interface between Consensus and Network layers.

use crate::counters;
use anyhow::anyhow;
use aptos_config::network_id::{NetworkId, PeerNetworkId};
use aptos_logger::prelude::*;
use aptos_types::{epoch_change::EpochChangeProof, PeerId};
use async_trait::async_trait;
use channel::{aptos_channel, message_queues::QueueStyle};
use consensus_types::{
    block_retrieval::{BlockRetrievalRequest, BlockRetrievalResponse},
    epoch_retrieval::EpochRetrievalRequest,
    experimental::{commit_decision::CommitDecision, commit_vote::CommitVote},
    proposal_msg::ProposalMsg,
    sync_info::SyncInfo,
    vote_msg::VoteMsg,
};
use network::{
    application::storage::PeerMetadataStorage,
    constants::NETWORK_CHANNEL_SIZE,
    error::NetworkError,
    peer_manager::{ConnectionRequestSender, PeerManagerRequestSender},
    protocols::{
        network::{
            AppConfig, ApplicationNetworkSender, NetworkEvents, NetworkSender, NewNetworkSender,
        },
        rpc::error::RpcError,
        wire::handshake::v1::ProtocolIdSet,
    },
    ProtocolId,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// Network type for consensus
/// 共识网络类型
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ConsensusMsg {
    /// RPC to get a chain of block of the given length starting from the given block id.
    /// RPC 从给定的块 id 获取给定长度的块链。
    BlockRetrievalRequest(Box<BlockRetrievalRequest>),
    /// Carries the returned blocks and the retrieval status.
    /// 携带返回的块和检索状态
    BlockRetrievalResponse(Box<BlockRetrievalResponse>),
    /// Request to get a EpochChangeProof from current_epoch to target_epoch
    /// 请求从 current_epoch 到 target_epoch 获取 EpochChangeProof
    EpochRetrievalRequest(Box<EpochRetrievalRequest>),
    /// ProposalMsg contains the required information for the proposer election protocol to make
    /// its choice (typically depends on round and proposer info).
    /// ProposalMsg 包含提议者选举协议做出选择所需的信息（通常取决于轮次和提议者信息）。
    ProposalMsg(Box<ProposalMsg>),
    /// This struct describes basic synchronization metadata.
    /// 这个结构描述了基本的同步元数据
    SyncInfo(Box<SyncInfo>),
    /// A vector of LedgerInfo with contiguous increasing epoch numbers to prove a sequence of
    /// epoch changes from the first LedgerInfo's epoch.
    /// LedgerInfo 的向量具有连续增加的纪元数，以证明从第一个 LedgerInfo 纪元开始的纪元序列变化。
    EpochChangeProof(Box<EpochChangeProof>),
    /// VoteMsg is the struct that is ultimately sent by the voter in response for receiving a
    /// proposal.
    /// VoteMsg 是最终由选民发送的结构，以响应接收提案。
    VoteMsg(Box<VoteMsg>),
    /// CommitProposal is the struct that is sent by the validator after execution to propose
    /// on the committed state hash root.
    /// CommitProposal 是验证器在执行后发送的结构体，用于在已提交状态哈希根上提出建议。
    CommitVoteMsg(Box<CommitVote>),
    /// CommitDecision is the struct that is sent by the validator after collecting no fewer
    /// than 2f + 1 signatures on the commit proposal. This part is not on the critical path, but
    /// it can save slow machines to quickly confirm the execution result.
    /// CommitDecision 是验证者在对提交提案收集不少于 2f + 1 个签名后发送的结构。
    /// 这部分不在关键路径上，但可以省去慢机器快速确认执行结果。
    CommitDecisionMsg(Box<CommitDecision>),
}

/// The interface from Network to Consensus layer.
///
/// `ConsensusNetworkEvents` is a `Stream` of `PeerManagerNotification` where the
/// raw `Bytes` direct-send and rpc messages are deserialized into
/// `ConsensusMessage` types. `ConsensusNetworkEvents` is a thin wrapper around
/// an `channel::Receiver<PeerManagerNotification>`.
/// 从网络到共识层的接口。
/// `ConsensusNetworkEvents` 是 `PeerManagerNotification` 的 `Stream`，
/// 其中原始的 `Bytes` 直接发送和 rpc 消息被反序列化为 `ConsensusMessage` 类型。
/// `ConsensusNetworkEvents` 是 `channel::Receiver<PeerManagerNotification>` 的薄包装。
pub type ConsensusNetworkEvents = NetworkEvents<ConsensusMsg>;

/// The interface from Consensus to Networking layer.
///
/// This is a thin wrapper around a `NetworkSender<ConsensusMsg>`, so it is easy
/// to clone and send off to a separate task. For example, the rpc requests
/// return Futures that encapsulate the whole flow, from sending the request to
/// remote, to finally receiving the response and deserializing. It therefore
/// makes the most sense to make the rpc call on a separate async task, which
/// requires the `ConsensusNetworkSender` to be `Clone` and `Send`.
/// .从共识层到网络层的接口。
/// 这是一个围绕 `NetworkSender<ConsensusMsg>` 的薄包装，因此很容易克隆并发送到单独的任务。
/// 例如，rpc 请求返回的 Futures 封装了整个流程，从发送请求到远程，到最终接收响应和反序列化。
/// 因此，在单独的异步任务上进行 rpc 调用是最有意义的，这需要 `ConsensusNetworkSender` 为 `Clone` 和 `Send`。
#[derive(Clone)]
pub struct ConsensusNetworkSender {
    network_sender: NetworkSender<ConsensusMsg>,
    peer_metadata_storage: Option<Arc<PeerMetadataStorage>>,
}

/// Supported protocols in preferred order (from highest priority to lowest).
/// 按优先顺序（从最高优先级到最低优先级）支持的协议。优先json
pub const RPC: &[ProtocolId] = &[ProtocolId::ConsensusRpcJson, ProtocolId::ConsensusRpcBcs];
/// Supported protocols in preferred order (from highest priority to lowest).
/// 按优先顺序（从最高优先级到最低优先级）支持的协议。
pub const DIRECT_SEND: &[ProtocolId] = &[
    ProtocolId::ConsensusDirectSendJson,
    ProtocolId::ConsensusDirectSendBcs,
];

/// Configuration for the network endpoints to support consensus.
/// TODO: make this configurable
pub fn network_endpoint_config() -> AppConfig {
    let protos = RPC.iter().chain(DIRECT_SEND.iter()).copied();
    AppConfig::p2p(
        protos,
        aptos_channel::Config::new(NETWORK_CHANNEL_SIZE)
            .queue_style(QueueStyle::LIFO)
            .counters(&counters::PENDING_CONSENSUS_NETWORK_EVENTS),
    )
}

impl NewNetworkSender for ConsensusNetworkSender {
    /// Returns a Sender that only sends for the `CONSENSUS_DIRECT_SEND_PROTOCOL` and
    /// `CONSENSUS_RPC_PROTOCOL` ProtocolId.
    fn new(
        peer_mgr_reqs_tx: PeerManagerRequestSender,
        connection_reqs_tx: ConnectionRequestSender,
    ) -> Self {
        Self {
            network_sender: NetworkSender::new(peer_mgr_reqs_tx, connection_reqs_tx),
            peer_metadata_storage: None,
        }
    }
}

impl ConsensusNetworkSender {
    /// Initialize a shared hashmap about connections metadata that is updated by the receiver.
    /// 初始化一个由接收者更新的关于连接元数据的共享哈希图。
    pub fn initialize(&mut self, peer_metadata_storage: Arc<PeerMetadataStorage>) {
        self.peer_metadata_storage = Some(peer_metadata_storage);
    }

    /// Query the supported protocols from this peer's connection.
    /// 从这个对等点的连接中查询支持的协议
    fn supported_protocols(&self, peer: PeerId) -> anyhow::Result<ProtocolIdSet> {
        if let Some(peer_metadata_storage) = &self.peer_metadata_storage {
            let peer_network_id = PeerNetworkId::new(NetworkId::Validator, peer);
            peer_metadata_storage
                .read(peer_network_id)
                .map(|peer_info| peer_info.active_connection.application_protocols)
                .ok_or_else(|| anyhow!("Peer not connected"))
        } else {
            Err(anyhow!("ConsensusNetworkSender not initialized"))
        }
    }

    /// Choose the overlapping protocol for peer. The local protocols are sorted from most to least preferred.
    /// 为对等选择重叠协议。本地协议按优先级从高到低排序。
    fn preferred_protocol_for_peer(
        &self,
        peer: PeerId,
        local_protocols: &[ProtocolId],
    ) -> anyhow::Result<ProtocolId> {
        let remote_protocols = self.supported_protocols(peer)?;
        for protocol in local_protocols {
            if remote_protocols.contains(*protocol) {
                return Ok(*protocol);
            }
        }
        Err(anyhow!("No available protocols for peer {}", peer))
    }
}

#[async_trait]
impl ApplicationNetworkSender<ConsensusMsg> for ConsensusNetworkSender {
    /// Send a single message to the destination peer using available ProtocolId.
    /// 使用可用的 ProtocolId 向目标对等方发送一条消息。
    fn send_to(&self, recipient: PeerId, message: ConsensusMsg) -> Result<(), NetworkError> {
        let protocol = self.preferred_protocol_for_peer(recipient, DIRECT_SEND)?;
        self.network_sender.send_to(recipient, protocol, message)
    }

    /// Send a single message to the destination peers using available ProtocolId.
    /// 使用可用的 ProtocolId 向目标对等方发送一条消息。
    fn send_to_many(
        &self,
        recipients: impl Iterator<Item = PeerId>,
        message: ConsensusMsg,
    ) -> Result<(), NetworkError> {
        let mut peers_per_protocol = HashMap::new();
        for peer in recipients {
            match self.preferred_protocol_for_peer(peer, DIRECT_SEND) {
                Ok(protocol) => peers_per_protocol
                    .entry(protocol)
                    .or_insert_with(Vec::new)
                    .push(peer),
                Err(e) => error!("{}", e),
            }
        }
        for (protocol, peers) in peers_per_protocol {
            self.network_sender
                .send_to_many(peers.into_iter(), protocol, message.clone())?;
        }
        Ok(())
    }

    /// Send a RPC to the destination peer using the `CONSENSUS_RPC_PROTOCOL` ProtocolId.
    async fn send_rpc(
        &self,
        recipient: PeerId,
        message: ConsensusMsg,
        timeout: Duration,
    ) -> Result<ConsensusMsg, RpcError> {
        let protocol = self.preferred_protocol_for_peer(recipient, RPC)?;
        self.network_sender
            .send_rpc(recipient, protocol, message, timeout)
            .await
    }
}
