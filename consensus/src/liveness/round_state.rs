// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters,
    pending_votes::{PendingVotes, VoteReceptionResult},
    util::time_service::{SendTask, TimeService},
};
use aptos_logger::{prelude::*, Schema};
use aptos_types::validator_verifier::ValidatorVerifier;
use consensus_types::{common::Round, sync_info::SyncInfo, vote::Vote};
use futures::future::AbortHandle;
use serde::Serialize;
use std::{fmt, sync::Arc, time::Duration};

/// A reason for starting a new round: introduced for monitoring / debug purposes.
/// 开始新一轮的原因：引入用于监控/调试目的
#[derive(Serialize, Eq, Debug, PartialEq)]
pub enum NewRoundReason {
    QCReady,
    Timeout,
}

impl fmt::Display for NewRoundReason {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NewRoundReason::QCReady => write!(f, "QCReady"),
            NewRoundReason::Timeout => write!(f, "TCReady"),
        }
    }
}

/// NewRoundEvents produced by RoundState are guaranteed to be monotonically increasing.
/// NewRoundEvents are consumed by the rest of the system: they can cause sending new proposals
/// or voting for some proposals that wouldn't have been voted otherwise.
/// The duration is populated for debugging and testing
/// RoundState 产生的 NewRoundEvents 保证单调递增。
/// NewRoundEvents 被系统的其余部分使用：它们可以导致发送新提案或为一些本来不会被投票的提案投票。为调试和测试填充持续时间
#[derive(Debug, PartialEq, Eq)]
pub struct NewRoundEvent {
    pub round: Round,
    pub reason: NewRoundReason,
    pub timeout: Duration,
}

impl fmt::Display for NewRoundEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "NewRoundEvent: [round: {}, reason: {}, timeout: {:?}]",
            self.round, self.reason, self.timeout
        )
    }
}

/// Determines the maximum round duration based on the round difference between the current
/// round and the committed round
/// 根据当前轮次和已提交轮次之间的轮次差异确定最大轮次持续时间
pub trait RoundTimeInterval: Send + Sync + 'static {
    /// Use the index of the round after the highest quorum certificate to commit a block and
    /// return the duration for this round
    ///
    /// Round indices start at 0 (round index = 0 is the first round after the round that led
    /// to the highest committed round).  Given that round r is the highest round to commit a
    /// block, then round index 0 is round r+1.  Note that for genesis does not follow the
    /// 3-chain rule for commits, so round 1 has round index 0.  For example, if one wants
    /// to calculate the round duration of round 6 and the highest committed round is 3 (meaning
    /// the highest round to commit a block is round 5, then the round index is 0.
    /// 使用最高法定人数证书之后的轮索引来提交一个块并返回该轮的持续时间
    /// 轮次索引从 0 开始（轮次索引 = 0 是导致最高承诺轮次的轮次之后的第一轮）。鉴于第 r 轮是提交区块的最高轮，则第 0 轮是第 r+1 轮。
    /// 请注意，对于 genesis 不遵循提交的 3 链规则，因此第 1 轮的轮索引为 0。
    /// 例如，如果要计算第 6 轮的轮持续时间，最高提交的轮是 3（意味着最高轮到提交一个块是第 5 轮，然后轮索引为 0
    fn get_round_duration(&self, round_index_after_committed_qc: usize) -> Duration;
}

/// Round durations increase exponentially
/// Basically time interval is base * mul^power
/// Where power=max(rounds_since_qc, max_exponent)
/// 回合持续时间呈指数增长
/// 基本上时间间隔是基础 mul^power
/// 其中 power=max(rounds_since_qc, max_exponent)
#[derive(Clone)]
pub struct ExponentialTimeInterval {
    // Initial time interval duration after a successful quorum commit.
    // 成功qc后的初始时间间隔持续时间。
    base_ms: u64,
    // By how much we increase interval every time
    // 我们每次增加多少间隔
    exponent_base: f64,
    // Maximum time interval won't exceed base * mul^max_pow.
    // Theoretically, setting it means
    // that we rely on synchrony assumptions when the known max messaging delay is
    // max_interval.  Alternatively, we can consider using max_interval to meet partial synchrony
    // assumptions where while delta is unknown, it is <= max_interval.
    // 最大时间间隔不会超过基本 mul^max_pow。
    // 从理论上讲，设置它意味着当已知的最大消息延迟为 max_interval 时，我们依赖于同步假设。
    // 或者，我们可以考虑使用 max_interval 来满足部分同步假设，其中虽然 delta 未知，但它 <= max_interval。
    max_exponent: usize,
}

impl ExponentialTimeInterval {
    #[cfg(any(test, feature = "fuzzing"))]
    pub fn fixed(duration: Duration) -> Self {
        Self::new(duration, 1.0, 0)
    }

    pub fn new(base: Duration, exponent_base: f64, max_exponent: usize) -> Self {
        assert!(
            max_exponent < 32,
            "max_exponent for RoundStateTimeInterval should be <32"
        );
        assert!(
            exponent_base.powf(max_exponent as f64).ceil() < f64::from(std::u32::MAX),
            "Maximum interval multiplier should be less then u32::Max"
        );
        ExponentialTimeInterval {
            base_ms: base.as_millis() as u64, // any reasonable ms timeout fits u64 perfectly
            exponent_base,
            max_exponent,
        }
    }
}

impl RoundTimeInterval for ExponentialTimeInterval {
    // (exponent_base ^ min(round, max_exponent)) * base_ms
    fn get_round_duration(&self, round_index_after_committed_qc: usize) -> Duration {
        let pow = round_index_after_committed_qc.min(self.max_exponent) as u32;
        let base_multiplier = self.exponent_base.powf(f64::from(pow));
        let duration_ms = ((self.base_ms as f64) * base_multiplier).ceil() as u64;
        Duration::from_millis(duration_ms)
    }
}

/// `RoundState` contains information about a specific round and moves forward when
/// receives new certificates.
///
/// A round `r` starts in the following cases:
/// * there is a QuorumCert for round `r-1`,
/// * there is a TimeoutCertificate for round `r-1`.
///
/// Round interval calculation is the responsibility of the RoundStateTimeoutInterval trait. It
/// depends on the delta between the current round and the highest committed round (the intuition is
/// that we want to exponentially grow the interval the further the current round is from the last
/// committed round).
///
/// Whenever a new round starts a local timeout is set following the round interval. This local
/// timeout is going to send the timeout events once in interval until the new round starts.
/// `RoundState` 包含有关特定回合的信息，并在收到新证书时向前移动。
/// 一轮“r”在以下情况下开始：
///     轮“r-1”有一个 QuorumCert，
///     “r-1”轮有一个 TimeoutCertificate。
/// 轮次间隔计算是 RoundStateTimeoutInterval trait 的职责。
/// 它取决于当前轮次和最高提交轮次之间的增量（直觉是，我们希望当前轮次距离上一次提交轮次越远，我们希望以指数方式增长间隔）。
/// 每当新一轮开始时，都会在轮次间隔之后设置本地超时。此本地超时将每隔一段时间发送一次超时事件，直到新一轮开始。
pub struct RoundState {
    // Determines the time interval for a round given the number of non-committed rounds since
    // last commit.
    // 在给定自上次提交以来未提交的轮数的情况下确定一轮的时间间隔
    time_interval: Box<dyn RoundTimeInterval>,
    // Highest known committed round as reported by the caller. The caller might choose not to
    // inform the RoundState about certain committed rounds (e.g., NIL blocks): in this case the
    // committed round in RoundState might lag behind the committed round of a block tree.
    // 调用者报告的已知最高已提交回合。调用者可能选择不通知 RoundState 某些已提交的轮次（例如，NIL 块）：
    // 在这种情况下，RoundState 中的已提交轮次可能落后于块树的已提交轮次。
    highest_committed_round: Round,
    // Current round is max{highest_qc, highest_tc} + 1.
    // 当前轮是 max（qc， tc） + 1
    current_round: Round,
    // The deadline for the next local timeout event. It is reset every time a new round start, or
    // a previous deadline expires.
    // Represents as Duration since UNIX_EPOCH.
    // 下一个本地超时事件的截止日期。每次新一轮开始或前一个截止日期到期时都会重置。表示为自 UNIX_EPOCH 以来的持续时间。
    current_round_deadline: Duration,
    // Service for timer
    time_service: Arc<dyn TimeService>,
    // To send local timeout events to the subscriber (e.g., SMR)
    timeout_sender: channel::Sender<Round>,
    // Votes received fot the current round.
    pending_votes: PendingVotes,
    // Vote sent locally for the current round.
    vote_sent: Option<Vote>,
    // The handle to cancel previous timeout task when moving to next round.
    abort_handle: Option<AbortHandle>,
}

#[derive(Default, Schema)]
pub struct RoundStateLogSchema<'a> {
    round: Option<Round>,
    committed_round: Option<Round>,
    #[schema(display)]
    pending_votes: Option<&'a PendingVotes>,
    #[schema(display)]
    self_vote: Option<&'a Vote>,
}

impl<'a> RoundStateLogSchema<'a> {
    pub fn new(state: &'a RoundState) -> Self {
        Self {
            round: Some(state.current_round),
            committed_round: Some(state.highest_committed_round),
            pending_votes: Some(&state.pending_votes),
            self_vote: state.vote_sent.as_ref(),
        }
    }
}

impl RoundState {
    pub fn new(
        time_interval: Box<dyn RoundTimeInterval>,
        time_service: Arc<dyn TimeService>,
        timeout_sender: channel::Sender<Round>,
    ) -> Self {
        // Our counters are initialized lazily, so they're not going to appear in
        // Prometheus if some conditions never happen. Invoking get() function enforces creation.
        counters::QC_ROUNDS_COUNT.get();
        counters::TIMEOUT_ROUNDS_COUNT.get();
        counters::TIMEOUT_COUNT.get();

        Self {
            time_interval,
            highest_committed_round: 0,
            current_round: 0,
            current_round_deadline: time_service.get_current_timestamp(),
            time_service,
            timeout_sender,
            pending_votes: PendingVotes::new(),
            vote_sent: None,
            abort_handle: None,
        }
    }

    /// Return the current round.
    pub fn current_round(&self) -> Round {
        self.current_round
    }

    /// Returns deadline for current round
    pub fn current_round_deadline(&self) -> Duration {
        self.current_round_deadline
    }

    /// In case the local timeout corresponds to the current round, reset the timeout and
    /// return true. Otherwise ignore and return false.
    pub fn process_local_timeout(&mut self, round: Round) -> bool {
        if round != self.current_round {
            return false;
        }
        warn!(round = round, "Local timeout");
        counters::TIMEOUT_COUNT.inc();
        self.setup_timeout();
        true
    }

    /// Notify the RoundState about the potentially new QC, TC, and highest committed round.
    /// Note that some of these values might not be available by the caller.
    pub fn process_certificates(&mut self, sync_info: SyncInfo) -> Option<NewRoundEvent> {
        if sync_info.highest_ordered_round() > self.highest_committed_round {
            self.highest_committed_round = sync_info.highest_ordered_round();
        }
        let new_round = sync_info.highest_round() + 1;
        if new_round > self.current_round {
            // Start a new round.
            self.current_round = new_round;
            self.pending_votes = PendingVotes::new();
            self.vote_sent = None;
            let timeout = self.setup_timeout();
            // The new round reason is QCReady in case both QC.round + 1 == new_round, otherwise
            // it's Timeout and TC.round + 1 == new_round.
            // 判断新一轮的原因，比如qc.round + 1 = new_round;或着tc.round + 1 = new_round
            let new_round_reason = if sync_info.highest_certified_round() + 1 == new_round {
                NewRoundReason::QCReady
            } else {
                NewRoundReason::Timeout
            };
            let new_round_event = NewRoundEvent {
                round: self.current_round,
                reason: new_round_reason,
                timeout,
            };
            debug!(round = new_round, "Starting new round: {}", new_round_event);
            return Some(new_round_event);
        }
        None
    }

    pub fn insert_vote(
        &mut self,
        vote: &Vote,
        verifier: &ValidatorVerifier,
    ) -> VoteReceptionResult {
        if vote.vote_data().proposed().round() == self.current_round {
            self.pending_votes.insert_vote(vote, verifier)
        } else {
            VoteReceptionResult::UnexpectedRound(
                vote.vote_data().proposed().round(),
                self.current_round,
            )
        }
    }

    pub fn record_vote(&mut self, vote: Vote) {
        if vote.vote_data().proposed().round() == self.current_round {
            self.vote_sent = Some(vote);
        }
    }

    pub fn vote_sent(&self) -> Option<Vote> {
        self.vote_sent.clone()
    }

    /// Setup the timeout task and return the duration of the current timeout
    /// 设置超时任务并返回当前超时的持续时间
    fn setup_timeout(&mut self) -> Duration {
        let timeout_sender = self.timeout_sender.clone();
        let timeout = self.setup_deadline();
        trace!(
            "Scheduling timeout of {} ms for round {}",
            timeout.as_millis(),
            self.current_round
        );
        let abort_handle = self
            .time_service
            .run_after(timeout, SendTask::make(timeout_sender, self.current_round));
        if let Some(handle) = self.abort_handle.replace(abort_handle) {
            handle.abort();
        }
        timeout
    }

    /// Setup the current round deadline and return the duration of the current round
    /// 设置当前回合截止日期并返回当前回合的持续时间
    fn setup_deadline(&mut self) -> Duration {
        let round_index_after_committed_round = {
            if self.highest_committed_round == 0 {
                // Genesis doesn't require the 3-chain rule for commit, hence start the index at
                // the round after genesis.
                self.current_round - 1
            } else if self.current_round < self.highest_committed_round + 3 {
                0
            } else {
                self.current_round - self.highest_committed_round - 3
            }
        } as usize;
        let timeout = self
            .time_interval
            .get_round_duration(round_index_after_committed_round);
        let now = self.time_service.get_current_timestamp();
        debug!(
            round = self.current_round,
            "{:?} passed since the previous deadline.",
            now.checked_sub(self.current_round_deadline)
                .map_or("0 ms".to_string(), |v| format!("{:?}", v))
        );
        debug!(
            round = self.current_round,
            "Set round deadline to {:?} from now", timeout
        );
        self.current_round_deadline = now + timeout;
        timeout
    }
}
