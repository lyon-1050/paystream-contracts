// SPDX-License-Identifier: Apache-2.0

//! Chaos testing for randomly interleaved employer/employee operations
//! (TEST-36, issue #331).
//!
//! The rest of the test-suite exercises one contract function at a time. This
//! module instead generates *random sequences* of mixed operations —
//! `create`, `top_up`, `withdraw`, `pause`, `resume`, `update_rate`, `cancel`
//! and time advances — and asserts the stream accounting invariants after
//! **every** step:
//!
//! * `withdrawn <= deposit` — the employee can never take out more than the
//!   employer put in.
//! * `claimable <= remaining` — the amount the contract reports as claimable
//!   never exceeds what is still owed.
//! * `!locked` — the re-entrancy guard is always released.
//!
//! The generator is a deterministic xorshift64\* PRNG so a failing run can be
//! reproduced exactly from its seed. When a sequence violates an invariant the
//! test shrinks the sequence down to a minimal failing case (shortest failing
//! prefix, then greedy removal of operations that are not needed to reproduce
//! the violation) and prints it in the panic message.
//!
//! This module is compiled into the crate's unit tests, so it runs as part of
//! the regular `cargo test` job in CI — not only in the dedicated fuzz job.
//!
//! The contract crate is `no_std`, so this module sticks to `core` types
//! (fixed-size arrays instead of `Vec`, no `String`/`format!`).

#![cfg(test)]

use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, Env};

use crate::types::StreamStatus;
use crate::{StreamContract, StreamContractClient};

/// Number of random operation sequences executed by the chaos test.
const SEQUENCES: u64 = 500;

/// Number of operations in every generated sequence.
const MAX_OPS: usize = 24;

/// Smallest deposit accepted by the chaos test (the contract's minimum
/// deposit is lowered to this value in the setup).
const MIN_DEPOSIT: i128 = 100;

/// Largest deposit used by the chaos test.
const MAX_DEPOSIT: i128 = 1_000;

/// Largest top-up used by the chaos test.
const MAX_TOP_UP: i128 = 500;

/// Largest rate used by the chaos test.
const MAX_RATE: i128 = 20;

/// Largest time advance (in seconds) applied by a single operation.
const MAX_ADVANCE: u64 = 60;

/// Deterministic xorshift64\* pseudo random number generator.
///
/// A hand-rolled generator keeps the chaos test free of extra dependencies
/// and makes every run reproducible: the same seed always produces the same
/// sequence of operations.
struct Rng {
    state: u64,
}

impl Rng {
    /// Create a generator for `seed`. Zero is remapped to a non-zero state
    /// because xorshift cannot escape the zero state.
    fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    /// Return the next pseudo random 64-bit value.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Return a pseudo random value in `[0, bound)`.
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }

    /// Return a pseudo random value in `[low, high]`.
    fn range(&mut self, low: i128, high: i128) -> i128 {
        let span = (high - low + 1) as u64;
        low + self.below(span) as i128
    }
}

/// A single operation in a generated chaos sequence.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// Create a new stream and make it the stream under test.
    Create { deposit: i128, rate: i128 },
    /// Add funds to the stream under test.
    TopUp { amount: i128 },
    /// Withdraw the accrued tokens of the stream under test.
    Withdraw,
    /// Pause the stream under test.
    Pause,
    /// Resume the stream under test.
    Resume,
    /// Change the rate of the stream under test.
    UpdateRate { rate: i128 },
    /// Cancel the stream under test.
    Cancel,
    /// Advance the ledger timestamp.
    Advance { seconds: u64 },
}

/// The invariant that a generated sequence violated.
///
/// The payload is only read through its `Debug` rendering in the failure
/// message, which the dead-code lint does not count as a read.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
enum ViolationKind {
    /// `withdrawn` exceeded `deposit`.
    WithdrawnExceedsDeposit { withdrawn: i128, deposit: i128 },
    /// The reported claimable amount exceeded the remaining deposit.
    ClaimableExceedsRemaining { claimable: i128, remaining: i128 },
    /// The re-entrancy guard was still set after the operation.
    StillLocked,
}

/// An invariant violation together with the operation that caused it.
#[derive(Clone, Copy, Debug)]
struct Violation {
    /// Index of the offending operation inside the sequence.
    op_index: usize,
    /// The invariant that was broken.
    kind: ViolationKind,
}

/// Generate a random sequence of operations.
fn generate_sequence(rng: &mut Rng) -> [Op; MAX_OPS] {
    let mut ops = [Op::Advance { seconds: 0 }; MAX_OPS];
    for slot in ops.iter_mut() {
        *slot = match rng.below(100) {
            0..=14 => Op::Create {
                deposit: rng.range(MIN_DEPOSIT, MAX_DEPOSIT),
                rate: rng.range(1, MAX_RATE),
            },
            15..=29 => Op::TopUp {
                amount: rng.range(1, MAX_TOP_UP),
            },
            30..=54 => Op::Withdraw,
            55..=64 => Op::Pause,
            65..=74 => Op::Resume,
            75..=84 => Op::UpdateRate {
                rate: rng.range(1, MAX_RATE),
            },
            85..=92 => Op::Cancel,
            _ => Op::Advance {
                seconds: rng.range(1, MAX_ADVANCE as i128) as u64,
            },
        };
    }
    ops
}

/// Set up a contract instance with a funded employer, an employee and a token.
fn setup() -> (
    Env,
    StreamContractClient<'static>,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(StreamContract, ());
    let client = StreamContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let employer = Address::generate(&env);
    let employee = Address::generate(&env);

    // The employer receives the initial token supply, so it can fund every
    // deposit and top-up generated by the chaos sequences.
    let token_id = env.register(paystream_token::TokenContract, ());
    let token = paystream_token::TokenContractClient::new(&env, &token_id);
    token.initialize(&employer, &1_000_000_000);

    client.initialize(&admin);
    // Lower the minimum deposit so the generated deposits are accepted.
    client.set_min_deposit(&admin, &0, &MIN_DEPOSIT);

    (env, client, employer, employee, token_id)
}

/// Check the accounting invariants of a single stream.
fn check_invariants(
    client: &StreamContractClient<'_>,
    stream_id: u64,
) -> Result<(), ViolationKind> {
    let stream = client.get_stream(&stream_id);

    if stream.withdrawn > stream.deposit {
        return Err(ViolationKind::WithdrawnExceedsDeposit {
            withdrawn: stream.withdrawn,
            deposit: stream.deposit,
        });
    }

    let remaining = stream.deposit - stream.withdrawn;
    let claimable = client.claimable(&stream_id);
    if claimable > remaining {
        return Err(ViolationKind::ClaimableExceedsRemaining {
            claimable,
            remaining,
        });
    }

    if stream.locked {
        return Err(ViolationKind::StillLocked);
    }

    Ok(())
}

/// Run a sequence of operations and return the first invariant violation.
fn run_sequence(ops: &[Op]) -> Result<(), Violation> {
    let (env, client, employer, employee, token_id) = setup();

    // The stream under test starts as the first created stream.
    let mut stream_id: Option<u64> = None;

    for (index, op) in ops.iter().enumerate() {
        match *op {
            Op::Create { deposit, rate } => {
                // Give the employer a fresh per-ledger creation budget so the
                // sequence is never rejected by the creation rate limit.
                env.ledger().with_mut(|l| l.sequence_number += 1);
                let id = client
                    .create_stream(&employer, &employee, &token_id, &deposit, &rate, &0, &None);
                stream_id = Some(id);
            }
            Op::TopUp { amount } => {
                if let Some(id) = stream_id {
                    if is_toppable(&client, id) {
                        client.top_up(&employer, &id, &amount, &None);
                    }
                }
            }
            Op::Withdraw => {
                if let Some(id) = stream_id {
                    if is_withdrawable(&client, id) {
                        client.withdraw(&employee, &id);
                    }
                }
            }
            Op::Pause => {
                if let Some(id) = stream_id {
                    if is_active(&client, id) {
                        client.pause_stream(&employer, &id);
                    }
                }
            }
            Op::Resume => {
                if let Some(id) = stream_id {
                    if is_paused(&client, id) {
                        client.resume_stream(&employer, &id);
                    }
                }
            }
            Op::UpdateRate { rate } => {
                if let Some(id) = stream_id {
                    if is_toppable(&client, id) {
                        client.update_rate(&employer, &id, &rate);
                    }
                }
            }
            Op::Cancel => {
                if let Some(id) = stream_id {
                    if is_toppable(&client, id) {
                        client.cancel_stream(&employer, &id);
                    }
                }
            }
            Op::Advance { seconds } => {
                env.ledger().with_mut(|l| l.timestamp += seconds);
            }
        }

        if let Some(id) = stream_id {
            if let Err(kind) = check_invariants(&client, id) {
                return Err(Violation {
                    op_index: index,
                    kind,
                });
            }
        }
    }

    Ok(())
}

/// Return `true` when the stream exists and is Active.
fn is_active(client: &StreamContractClient<'_>, stream_id: u64) -> bool {
    client.get_stream(&stream_id).status == StreamStatus::Active
}

/// Return `true` when the stream exists and is Paused.
fn is_paused(client: &StreamContractClient<'_>, stream_id: u64) -> bool {
    client.get_stream(&stream_id).status == StreamStatus::Paused
}

/// Return `true` when the stream is Active or Paused — the statuses in which
/// top-ups, rate changes and cancellations are allowed.
fn is_toppable(client: &StreamContractClient<'_>, stream_id: u64) -> bool {
    let status = client.get_stream(&stream_id).status;
    status == StreamStatus::Active || status == StreamStatus::Paused
}

/// Return `true` when the stream is Active or Exhausted — the statuses in
/// which withdrawals are allowed.
fn is_withdrawable(client: &StreamContractClient<'_>, stream_id: u64) -> bool {
    let status = client.get_stream(&stream_id).status;
    status == StreamStatus::Active || status == StreamStatus::Exhausted
}

/// Shrink a failing sequence to the length of a minimal failing prefix.
///
/// First the shortest failing prefix is located, then individual operations
/// are greedily removed for as long as the violation is still reproducible.
/// The returned length is the number of operations of the minimal case.
fn minimize(ops: &[Op]) -> usize {
    let mut best_len = ops.len();
    for len in 1..ops.len() {
        if run_sequence(&ops[..len]).is_err() {
            best_len = len;
            break;
        }
    }

    let mut current = [Op::Advance { seconds: 0 }; MAX_OPS];
    current[..ops.len()].copy_from_slice(ops);

    let mut len = best_len;
    let mut index = 0;
    while index < len {
        let mut candidate = current;
        for position in index..len - 1 {
            candidate[position] = candidate[position + 1];
        }
        if run_sequence(&candidate[..len - 1]).is_err() {
            len -= 1;
            current = candidate;
        } else {
            index += 1;
        }
    }

    len
}

/// TEST-36 (#331): randomly interleaved employer/employee operations must
/// never break the stream accounting invariants.
///
/// 500 deterministic random sequences are executed. Every sequence starts
/// with a `create` operation, so there is always a stream to operate on.
#[test]
fn chaos_random_sequences_preserve_stream_invariants() {
    for seed in 0..SEQUENCES {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        let mut ops = generate_sequence(&mut rng);
        // Every sequence needs at least one stream to operate on.
        ops[0] = Op::Create {
            deposit: MIN_DEPOSIT,
            rate: 1,
        };

        if let Err(violation) = run_sequence(&ops) {
            let minimal_len = minimize(&ops);
            panic!(
                "chaos sequence {seed} violated a stream invariant.\n\
                 violation: {:?} after operation #{} ({:?})\n\
                 original sequence: {:?}\n\
                 minimal failing sequence: {:?}",
                violation.kind,
                violation.op_index,
                ops[violation.op_index],
                &ops[..],
                &ops[..minimal_len]
            );
        }
    }
}

/// TEST-36 (#331): the chaos generator must produce a mix of every supported
/// operation, otherwise the random sequences would not exercise the state
/// machine at all.
#[test]
fn chaos_generator_covers_every_operation() {
    let mut seen_create = false;
    let mut seen_top_up = false;
    let mut seen_withdraw = false;
    let mut seen_pause = false;
    let mut seen_resume = false;
    let mut seen_update_rate = false;
    let mut seen_cancel = false;
    let mut seen_advance = false;

    for seed in 0..SEQUENCES {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        for op in generate_sequence(&mut rng) {
            match op {
                Op::Create { .. } => seen_create = true,
                Op::TopUp { .. } => seen_top_up = true,
                Op::Withdraw => seen_withdraw = true,
                Op::Pause => seen_pause = true,
                Op::Resume => seen_resume = true,
                Op::UpdateRate { .. } => seen_update_rate = true,
                Op::Cancel => seen_cancel = true,
                Op::Advance { .. } => seen_advance = true,
            }
        }
    }

    assert!(seen_create, "generator never produced a create operation");
    assert!(seen_top_up, "generator never produced a top-up operation");
    assert!(
        seen_withdraw,
        "generator never produced a withdraw operation"
    );
    assert!(seen_pause, "generator never produced a pause operation");
    assert!(seen_resume, "generator never produced a resume operation");
    assert!(
        seen_update_rate,
        "generator never produced a rate update operation"
    );
    assert!(seen_cancel, "generator never produced a cancel operation");
    assert!(
        seen_advance,
        "generator never produced a time advance operation"
    );
}
