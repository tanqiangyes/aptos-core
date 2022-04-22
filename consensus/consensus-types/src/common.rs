// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use aptos_types::{account_address::AccountAddress, transaction::SignedTransaction};

/// The round of a block is a consensus-internal counter, which starts with 0 and increases
/// monotonically. It is used for the protocol safety and liveness (please see the detailed
/// protocol description).
pub type Round = u64;
/// Author refers to the author's account address
pub type Author = AccountAddress;

/// The payload in block.
/// 区块中的交易
pub type Payload = Vec<SignedTransaction>;
