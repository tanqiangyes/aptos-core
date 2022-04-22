// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use consensus_types::{
    block::Block,
    common::{Author, Round},
};
use fallible::copy_from_slice::copy_slice_to_vec;

/// ProposerElection incorporates the logic of choosing a leader among multiple candidates.
/// We are open to a possibility for having multiple proposers per round, the ultimate choice
/// of a proposal is exposed by the election protocol via the stream of proposals.
/// ProposerElection 结合了在多个候选人中选择领导者的逻辑。我们对每轮有多个提议者的可能性持开放态度，提议的最终选择由选举协议通过提议流公开。
pub trait ProposerElection {
    /// If a given author is a valid candidate for being a proposer, generate the info,
    /// otherwise return None.
    /// Note that this function is synchronous.
    /// 如果给定的作者是作为提议者的有效候选人，则生成信息，否则返回 None。请注意，此函数是同步的.
    fn is_valid_proposer(&self, author: Author, round: Round) -> bool {
        self.get_valid_proposer(round) == author
    }

    /// Return the valid proposer for a given round (this information can be
    /// used by e.g., voters for choosing the destinations for sending their votes to).
    /// 返回给定轮次的有效提议者（例如，选民可以使用此信息来选择发送选票的目的地）
    fn get_valid_proposer(&self, round: Round) -> Author;

    /// Return if a given proposed block is valid.
    /// 如果给定的提议块有效，则返回
    fn is_valid_proposal(&self, block: &Block) -> bool {
        block.author().map_or(false, |author| {
            self.is_valid_proposer(author, block.round())
        })
    }
}

// next continuously mutates a state and returns a u64-index
// next 不断地改变一个状态并返回一个 u64-index
pub(crate) fn next(state: &mut Vec<u8>) -> u64 {
    // state = SHA-3-256(state)
    *state = aptos_crypto::HashValue::sha3_256_of(state).to_vec();
    let mut temp = [0u8; 8];
    copy_slice_to_vec(&state[..8], &mut temp).expect("next failed");
    // return state[0..8]
    u64::from_le_bytes(temp)
}
