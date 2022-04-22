// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::liveness::proposer_election::ProposerElection;
use consensus_types::common::{Author, Round};

use std::collections::HashMap;

/// The round proposer maps a round to author
/// 回合提议者将回合映射到作者
pub struct RoundProposer {
    // A pre-defined map specifying proposers per round
    // 指定每轮提议者的预定义映射
    proposers: HashMap<Round, Author>,
    // Default proposer to use if proposer for a round is unspecified.
    // We hardcode this to the first proposer
    // 如果一轮的提议者未指定，则使用默认提议者。我们将其硬编码给第一个提议者
    default_proposer: Author,
}

impl RoundProposer {
    pub fn new(proposers: HashMap<Round, Author>, default_proposer: Author) -> Self {
        Self {
            proposers,
            default_proposer,
        }
    }
}

impl ProposerElection for RoundProposer {
    fn get_valid_proposer(&self, round: Round) -> Author {
        match self.proposers.get(&round) {
            None => self.default_proposer,
            Some(round_proposer) => *round_proposer,
        }
    }
}
