// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::liveness::proposer_election::ProposerElection;
use consensus_types::common::{Author, Round};

/// The rotating proposer maps a round to an author according to a round-robin rotation.
/// A fixed proposer strategy loses liveness when the fixed proposer is down. Rotating proposers
/// won't gather quorum certificates to machine loss/byzantine behavior on f/n rounds.
/// 轮换提议者根据循环轮换将一轮映射到作者。当固定提议者关闭时，固定提议者策略会失去活力。
/// 轮换提议者不会收集法定人数证书以在 f/n 轮中机器丢失/拜占庭行为。
pub struct RotatingProposer {
    // Ordering of proposers to rotate through (all honest replicas must agree on this)
    // 提议者轮换列表（所有诚实的副本都必须同意这一点）
    proposers: Vec<Author>,
    // Number of contiguous rounds (i.e. round numbers increase by 1) a proposer is active
    // in a row
    // 提议者连续活跃的连续轮数（即轮数增加 1）
    contiguous_rounds: u32,
}

/// Choose a proposer that is going to be the single leader (relevant for a mock fixed proposer
/// election only).
/// 选择一个将成为单一领导者的提议者（仅与模拟固定提议者选举相关）
pub fn choose_leader(peers: Vec<Author>) -> Author {
    // As it is just a tmp hack function, pick the min PeerId to be a proposer.
    peers.into_iter().min().expect("No trusted peers found!")
}

impl RotatingProposer {
    /// With only one proposer in the vector, it behaves the same as a fixed proposer strategy.
    pub fn new(proposers: Vec<Author>, contiguous_rounds: u32) -> Self {
        Self {
            proposers,
            contiguous_rounds,
        }
    }
}

impl ProposerElection for RotatingProposer {
    fn get_valid_proposer(&self, round: Round) -> Author {
        self.proposers
            [((round / u64::from(self.contiguous_rounds)) % self.proposers.len() as u64) as usize]
    }
}
