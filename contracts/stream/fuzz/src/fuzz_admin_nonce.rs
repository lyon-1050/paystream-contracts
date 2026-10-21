// SPDX-License-Identifier: Apache-2.0

//! Fuzz target for the admin nonce replay protection (SEC-02).
//!
//! Every admin-only entry point (`set_min_deposit`, `set_max_creates_per_ledger`,
//! `set_protocol_fee`, …) consumes the stored admin nonce, so the nonce is the
//! replay-protection boundary of the whole admin surface.
//!
//! This target drives the nonce through the public contract API (soroban-sdk
//! 28 forbids touching the instance storage from outside a contract call) and
//! verifies that:
//!
//! 1. Only the currently expected nonce is accepted; everything else panics
//!    with E009.
//! 2. After acceptance the stored nonce is exactly `n + 1`.
//! 3. Replaying an already consumed nonce panics with E009.
//! 4. A sequence of strictly sequential nonces always succeeds and increments
//!    the counter exactly once per accepted call.
//! 5. A nonce that is ahead of the expected one is rejected as well.

#![allow(unused_imports, dead_code)]

use proptest::prelude::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env};

use paystream_stream::{StreamContract, StreamContractClient};

/// Set up a contract instance with `admin` as the stored admin.
fn setup() -> (Env, StreamContractClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(StreamContract, ());
    let client = StreamContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    client.initialize(&admin);
    (env, client, admin)
}

/// Consume the admin nonce with `nonce` by setting the minimum deposit.
fn consume_nonce(client: &StreamContractClient<'_>, admin: &Address, nonce: u64) {
    client.set_min_deposit(admin, &nonce, &100);
}

/// Return the message of a caught panic.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else {
        String::new()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500_000))]

    /// Only the currently expected nonce is accepted; everything else panics
    /// with E009.  After acceptance the nonce increments by exactly 1.
    #[test]
    fn fuzz_admin_nonce_only_expected_accepted(
        // Generate a sequence of up to 8 nonce attempts before the correct one.
        wrong_attempts in prop::collection::vec(0u64..u64::MAX, 0..8),
        // A small starting nonce so we can run a few acceptance cycles.
        start_nonce in 0u64..100u64,
        // How many successful nonce advances to perform (1–4).
        advances in 1u64..=4u64,
    ) {
        let (_env, client, admin) = setup();

        // Consume the nonces below `start_nonce` so the counter starts there.
        for n in 0..start_nonce {
            consume_nonce(&client, &admin, n);
        }

        let mut expected = start_nonce;

        // --- Phase 1: wrong attempts must all panic with E009 ---
        for &wrong in &wrong_attempts {
            if wrong == expected {
                // Skip: this would be the correct nonce, not a replay attack.
                continue;
            }

            // Attempt to consume an incorrect nonce — must panic with E009.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Each catch_unwind needs a fresh contract instance because the
                // Soroban SDK may leave the environment in an inconsistent
                // state after a panic.
                let (_env2, client2, admin2) = setup();
                for n in 0..expected {
                    consume_nonce(&client2, &admin2, n);
                }
                consume_nonce(&client2, &admin2, wrong);
            }));

            prop_assert!(
                result.is_err(),
                "consume_admin_nonce({wrong}) should panic when expected nonce is {expected}"
            );

            // Verify the panic message contains E009.
            if let Err(e) = result {
                let msg = panic_message(e);
                prop_assert!(
                    msg.contains("E009"),
                    "panic message must contain E009, got: {msg:?}"
                );
            }
        }

        // --- Phase 2: correct nonce must succeed and increment atomically ---
        for _ in 0..advances {
            // Consume the correct nonce — must NOT panic.
            consume_nonce(&client, &admin, expected);

            // --- Phase 2b: replay of the just-consumed nonce must panic with E009 ---
            let replay_nonce = expected; // already consumed
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (_env3, client3, admin3) = setup();
                // Advance the counter to the post-advance state.
                for n in 0..=expected {
                    consume_nonce(&client3, &admin3, n);
                }
                // Replay: try to consume the same nonce again.
                consume_nonce(&client3, &admin3, replay_nonce);
            }));

            prop_assert!(
                result.is_err(),
                "replaying nonce {replay_nonce} must panic"
            );

            if let Err(e) = result {
                let msg = panic_message(e);
                prop_assert!(
                    msg.contains("E009"),
                    "panic message must contain E009, got: {msg:?}"
                );
            }

            expected += 1;
        }
    }

    /// A stream of purely sequential nonces (0, 1, 2, …) must all succeed.
    #[test]
    fn fuzz_admin_nonce_sequential_always_succeeds(
        count in 1u64..=20u64,
    ) {
        let (_env, client, admin) = setup();

        for n in 0..count {
            consume_nonce(&client, &admin, n);
        }
    }

    /// Any nonce other than the current one must be rejected with E009,
    /// regardless of whether it is less than or greater than the expected
    /// value.
    #[test]
    fn fuzz_admin_nonce_non_sequential_rejected(
        current in 0u64..=50u64,
        offset  in 1u64..=u64::MAX / 2,
    ) {
        let (_env, client, admin) = setup();

        for n in 0..current {
            consume_nonce(&client, &admin, n);
        }

        let ahead = current + offset;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (_env2, client2, admin2) = setup();
            for n in 0..current {
                consume_nonce(&client2, &admin2, n);
            }
            consume_nonce(&client2, &admin2, ahead);
        }));

        prop_assert!(result.is_err(), "nonce {ahead} must be rejected");
        if let Err(e) = result {
            let msg = panic_message(e);
            prop_assert!(
                msg.contains("E009"),
                "panic message must contain E009, got: {msg:?}"
            );
        }
    }
}

fn main() {}
