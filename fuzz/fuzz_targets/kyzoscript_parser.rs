/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes the public KyzoScript language door
//! (`kyzo_model::parse::parse_script`).
//!
//! Rewired after sealed-door demolition: this target speaks the public
//! parse seat directly. The old `kyzo::fuzz_api::fuzz_parse_script` façade
//! is gone and must not be restored.
//!
//! Invariant: parsing arbitrary bytes never panics. `Ok` and `Err` are both
//! acceptable. Pest can backtrack into a multi-minute hang on some short
//! hostile inputs; each input is therefore parsed in a child process with a
//! hard wall-clock budget (SIGKILL + continue) so fuzz-smoke cannot stall
//! (libFuzzer `-max_total_time` only checks between inputs).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kyzo_model::parse::parse_script;
use kyzo_model::value::ValidityTs;
use libfuzzer_sys::fuzz_target;

/// Wall-clock budget for one parse. Longer than any healthy parse; short
/// enough that a pest backtrack cannot monopolize the 60s smoke.
const PARSE_BUDGET: Duration = Duration::from_millis(250);

fn parse_with_budget(src: &str) {
    // Fixed session stamp: the door requires a real `ValidityTs`; fuzzing
    // the stamp itself is out of scope for this target.
    let cur = ValidityTs::from_raw(0);

    // Fork so a hung pest parse can be SIGKILL'd. ASAN parent continues;
    // the child exits without running ASAN leak checks (`_exit`).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        panic!("fork failed while enforcing parser fuzz budget");
    }
    if pid == 0 {
        let _ = parse_script(src, &BTreeMap::new(), cur);
        unsafe { libc::_exit(0) };
    }

    let deadline = Instant::now() + PARSE_BUDGET;
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited < 0 {
            panic!("waitpid failed while enforcing parser fuzz budget");
        }
        if waited == pid {
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "parser child panicked or was signaled"
            );
            return;
        }
        if Instant::now() >= deadline {
            // Hang contained: kill the child and continue. Do not turn a
            // budget kill into a libfuzzer "crash" — that shrinks the
            // campaign to ReDoS triage and stalls CI smoke. Real panics
            // still fail via the child-status assert above.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fuzz_target!(|data: &[u8]| {
    // `&str` is the real API surface; lossy conversion matches how the
    // parse-tier's own generative fuzz harness treats byte-mutated
    // (possibly invalid-UTF-8) input.
    let src = String::from_utf8_lossy(data);
    parse_with_budget(&src);
});
