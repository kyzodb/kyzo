/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Peer-dial ban (decisions.md seats 18 / 92): NATS/JetStream is the only
//! nervous system. Kyzo has **no peer-connection type and no dial API** —
//! "fabric-down → `Refuse(FabricUnavailable)`", never a direct socket to
//! another node. A raw TCP/UDP socket or a listener anywhere in the engine
//! IS a second nervous system: the exact "overlay / mesh / object-sync"
//! second brain those seats delete.
//!
//! **Scope: the engine, not the host.** The ban applies to every pure engine
//! crate — `kyzo-core`, `kyzo-model`, `kyzo-trials` (the DST/crash-testing
//! harness), `kyzo-oracle` (the independent `::verify` judge), `kyzo-crashfs`
//! (fault injection), `kyzo-lsp`, and `kyzo-arrow-interop` — none of which own
//! IO at all (zone-model forbids sockets, clocks, and randomness outright). A
//! raw socket in any of them is, by definition, the second nervous system
//! seats 18/92 delete. Widened from the original two-crate list after an
//! audit found the other five engine crates undisclosed and unscanned by
//! this ban — the same gap `walk_engine_sources` had before it was widened.
//! The **host** binary (`kyzo-bin`) is the adapter boundary (seat 74): it
//! legitimately binds a client-facing HTTP API listener and runs HTTP client
//! utilities — those are inbound-client / resource-fetch sockets, NOT a
//! Kyzo-node peer dial, so it is out of this check's scope. `xtask` is also
//! out of scope: it is the gate's own tooling, not engine surface, and its
//! test fixtures parse synthetic socket-shaped source strings (e.g. this
//! file's own `TcpStream::connect` fixture) that would otherwise
//! self-trigger — the same documented reason `walk_engine_sources` blanks
//! xtask's `#[cfg(test)]` scopes before any check ever sees them, so this
//! check does not need its own second exclusion mechanism for xtask. The one
//! legal *fabric* transport (`async-nats`) is a dependency reached through
//! the object/fabric trait injected by the host — its sockets live in that
//! crate, never hand-rolled in the engine.
//!
//! Within engine scope there is **no allowlist**: seat 92 rules an engine peer
//! socket *unrepresentable*, so there is no exception to grant. Detection is a
//! path-segment scan (catches `use`, type position, and call position alike)
//! over the banned std/tokio socket primitives, skipping `#[cfg(test)]`
//! scopes — a fixture may bind a loopback to prove a refuse.

use crate::checks::banned_path::scan_banned_idents;
use crate::fsutil::SourceFile;

/// Banned socket primitives — a raw peer/transport connection the fabric law
/// forbids. Matched on any path segment so `std::net::TcpStream`,
/// `tokio::net::TcpStream`, and a bare `TcpStream` all trip alike.
const BANNED_SOCKET_TYPES: &[&str] = &[
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "UnixStream",
    "UnixListener",
];

/// One raw-socket site found in non-test engine code.
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub symbol: String,
}

/// True for every pure engine crate the ban governs. The host adapter
/// (`kyzo-bin`) legitimately owns client-facing sockets, and `xtask` (the
/// gate's own tooling, whose test fixtures parse synthetic socket-shaped
/// source strings) is out of scope — see module docs.
const ENGINE_CRATES: &[&str] = &[
    "crates/kyzo-core/",
    "crates/kyzo-model/",
    "crates/kyzo-trials/",
    "crates/kyzo-oracle/",
    "crates/kyzo-crashfs/",
    "crates/kyzo-lsp/",
    "crates/kyzo-arrow-interop/",
];

fn is_engine_scope(rel_path: &str) -> bool {
    ENGINE_CRATES.iter().any(|c| rel_path.starts_with(c))
}

/// Scan every pure-engine source for a hand-rolled peer/transport socket.
pub fn check(files: &[SourceFile]) -> Vec<Violation> {
    let mut violations = vec![];
    for f in files {
        if !is_engine_scope(&f.rel_path) {
            continue;
        }
        for hit in scan_banned_idents(f, BANNED_SOCKET_TYPES) {
            violations.push(Violation {
                file: f.rel_path.clone(),
                line: hit.line,
                symbol: hit.ident,
            });
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> SourceFile {
        SourceFile {
            rel_path: "crates/kyzo-core/src/probe.rs".to_string(),
            text: src.to_string(),
            ast: syn::parse_file(src).expect("fixture parses"),
        }
    }

    #[test]
    fn flags_a_raw_tcp_stream_dial() {
        let f = parse("fn dial() { let _c = std::net::TcpStream::connect(\"127.0.0.1:4222\"); }");
        let v = check(std::slice::from_ref(&f));
        assert_eq!(v.len(), 1, "a raw TcpStream dial must be caught");
        assert_eq!(v[0].symbol, "TcpStream");
    }

    #[test]
    fn flags_a_bare_listener_type() {
        let f = parse("use tokio::net::TcpListener;\nfn f(_l: TcpListener) {}");
        // one at the `use`, one at the type position
        let v = check(std::slice::from_ref(&f));
        assert!(v.len() >= 1, "a TcpListener reference must be caught");
        assert!(v.iter().all(|x| x.symbol == "TcpListener"));
    }

    #[test]
    fn ignores_a_test_scope_loopback() {
        let f = parse(
            "#[cfg(test)]\nmod tests { fn dial() { let _ = std::net::TcpStream::connect(\"x\"); } }",
        );
        assert!(
            check(std::slice::from_ref(&f)).is_empty(),
            "a #[cfg(test)] loopback fixture is out of scope"
        );
    }

    #[test]
    fn clean_engine_code_passes() {
        let f = parse("fn commit() -> u64 { 42 }");
        assert!(check(std::slice::from_ref(&f)).is_empty());
    }

    #[test]
    fn host_binary_client_socket_is_out_of_scope() {
        // The same raw socket that fails inside the engine is legal in the host
        // adapter (kyzo-bin) — a client-facing listener / fetch client, not a
        // Kyzo-node peer dial.
        let host = SourceFile {
            rel_path: "crates/kyzo-bin/src/server/mod.rs".to_string(),
            text: "fn serve() { let _l = std::net::TcpListener::bind(\"0:0\"); }".to_string(),
            ast: syn::parse_file("fn serve() { let _l = std::net::TcpListener::bind(\"0:0\"); }")
                .unwrap(),
        };
        assert!(
            check(std::slice::from_ref(&host)).is_empty(),
            "a host-adapter client socket must be out of the engine-only ban"
        );
    }

    #[test]
    fn flags_a_raw_socket_in_a_widened_engine_crate() {
        // kyzo-trials/kyzo-oracle/kyzo-crashfs/kyzo-lsp/kyzo-arrow-interop were
        // undisclosed before this widen — a raw dial in any of them must now
        // be caught exactly like kyzo-core/kyzo-model.
        for rel in [
            "crates/kyzo-trials/src/campaign.rs",
            "crates/kyzo-oracle/src/verify.rs",
            "crates/kyzo-crashfs/src/inject.rs",
            "crates/kyzo-lsp/src/server.rs",
            "crates/kyzo-arrow-interop/src/bridge.rs",
        ] {
            let src = "fn dial() { let _c = std::net::TcpStream::connect(\"127.0.0.1:4222\"); }";
            let f = SourceFile {
                rel_path: rel.to_string(),
                text: src.to_string(),
                ast: syn::parse_file(src).expect("fixture parses"),
            };
            let v = check(std::slice::from_ref(&f));
            assert_eq!(v.len(), 1, "{rel}: a raw TcpStream dial must be caught");
            assert_eq!(v[0].symbol, "TcpStream");
        }
    }
}
