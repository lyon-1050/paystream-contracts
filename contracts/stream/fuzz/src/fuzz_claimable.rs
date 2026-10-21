// SPDX-License-Identifier: Apache-2.0

#![allow(unused_imports, dead_code)]

use paystream_stream::storage::claimable_amount;
use paystream_stream::types::{Stream, StreamStatus};
use proptest::prelude::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env};

fn make_stream(
    env: &Env,
    deposit: i128,
    withdrawn: i128,
    rate_per_second: i128,
    last_withdraw_time: u64,
    stop_time: u64,
    status: StreamStatus,
) -> Stream {
    let addr = Address::generate(env);
    Stream {
        id: 1,
        employer: addr.clone(),
        employee: addr.clone(),
        token: addr,
        deposit,
        withdrawn,
        rate_per_second,
        start_time: 0,
        stop_time,
        last_withdraw_time,
        status,
        pending_accrual: 0,
        locked: false,
        low_balance_threshold: 0,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000_000))]

    /// claimable_amount must never panic (except the documented overflow case)
    /// and must always return a value in [0, deposit - withdrawn].
    #[test]
    fn fuzz_claimable_no_panic_and_bounded(
        deposit        in 0i128..=i64::MAX as i128,
        withdrawn      in 0i128..=i64::MAX as i128,
        rate           in 1i128..=1_000_000i128,
        last_withdraw  in 0u64..=u64::MAX / 2,
        stop_time      in 0u64..=u64::MAX / 2,
        now            in 0u64..=u64::MAX / 2,
    ) {
        let env = Env::default();
        // Ensure withdrawn <= deposit for a well-formed stream
        let withdrawn = withdrawn.min(deposit);
        let stream = make_stream(
            &env,
            deposit,
            withdrawn,
            rate,
            last_withdraw,
            stop_time,
            StreamStatus::Active,
        );
        let result = claimable_amount(&stream, now);
        let remaining = deposit - withdrawn;
        prop_assert!(result >= 0, "claimable must be non-negative");
        prop_assert!(result <= remaining, "claimable must not exceed remaining deposit");
    }

    /// Cancelled and Exhausted streams always return 0.
    #[test]
    fn fuzz_claimable_terminal_states_zero(
        deposit   in 1i128..=i64::MAX as i128,
        rate      in 1i128..=1_000_000i128,
        now       in 0u64..u64::MAX / 2,
    ) {
        let env = Env::default();
        for status in [StreamStatus::Cancelled, StreamStatus::Exhausted] {
            let stream = make_stream(&env, deposit, 0, rate, 0, 0, status);
            prop_assert_eq!(claimable_amount(&stream, now), 0);
        }
    }
}

fn main() {}
