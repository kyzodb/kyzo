# CLAUDE.md — KyzoDB

A pure Rust database that supports relational, graph, vector, full-text, geo, and temporal data models on a single ordered substrate under one law. Every durable write enters as a KyzoRecord (typed statement + accountability envelope: identity, provenance, authority, standing, bitemporal validity) and lowers deterministically into relations; graph, vector, text, and spatial access are projections, never separate truth. ONTOK ten conceptual primitives) is the grammar; OntologyPacks compile the world's curated ontologies into anchor embeddings + rules, so ingestion stays deterministic while records land *situated*. Queries are KyzoScript (Datalog): unique fixpoint, typed refusal, replayable proof. NATS-native, federated/encrypted p2p with query routing; FROST threshold recovery; key-committing AEAD; RaBitQ accelerator over exact-float truth; proven against oracles and crash/DST campaigns. Storage drivers include Cloudflare Durable Objects; full WASM planned — local and hosted must feel identical.
.
 `README.md` defines the product; the `KyzoDB Work` board defines the plan. CozoDB fork (`FORK.md`); licensing map in `LICENSING.md`.

## The Foundational Invariant of KyzoDB

Every stored value encodes to bytes whose binary order equals its semantic order — what lets one substrate serve every query model. A sort-order defect is silent wrong answers, so this contract is executable law, enforced mechanically (property tests, corruption harnesses, mutation, fuzzing). Never weaken that enforcement.

## How We Work
- We normally use KyzoPlan tools to do our work. 
- We use common sense and good judgment. 
- Timidity, hedging, and failing to try and do the best/right thing is harmful risk that is forbidden and always incorrect.

**PLEASE NOTE:** We are in an exception process while KyzoPlan is being updated. This is current process

Claude Code and Cursor work together via the message bus
Claude Code manages the board work and the Cursor development team.
The Cursor development team is not required to use KyzoPlan to work stories.
Defects found by the resonance system hit Claude with a hook. Claude flips the fail to pass and messages dev team to fix asap
We stop story work to fix detected defects.

## Message Bus Configuration

Claude<->Cursor messaging: `.kyzo/bus_msg.py put --from <claude|cursor> --to <cursor|claude> --kind <k> --task <t> --body "..."` / `.kyzo/bus_msg.py list --after <id>`. Backed by a real `agent_messages` relation on a live `kyzo server --engine fjall` (127.0.0.1:9077); embeds via local Ollama `granite-embedding:278m` (127.0.0.1:11434). Both must be running or the bus is dead.

**No push delivery — polling only.** `.kyzo/bus_watch.py` + `.kyzo/bus-arm.txt` (watermark id) is the monitor. This MUST be set up and kept running; if nobody is polling, messages sit unseen indefinitely. Claude must restart/verify its own monitor after any compaction, restart, or reported gap — do not assume a prior session's monitor is still alive.
