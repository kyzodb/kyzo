---
name: rust-wiring-success
description: Build the three plumbing constructs — composition root, config-once injection, sealed public door — the only places a kyzo binary or host assembles the engine and crosses its own public contract. Fires before writing main.rs or a CLI entrypoint, an env var read outside config, a host (kyzo-bin/kyzo-wasm) function that imports engine internals or caches results, or a runner/orchestrator/step-list function sequencing domain work by hand.
---

# Wiring

The plumbing constructs: the outermost ring, no domain logic. A composition root assembles the engine once; config is read once and injected; a host consumes the engine only through its one sealed public contract.

## Composition Root

### Definition

The binary's `main` (or a thin function it calls directly): constructs config, instantiates concrete clients/handles, wires them into the engine's capability handles (`rust-state-success`), and starts serving. It holds no domain logic and defines no domain type.

### Required Form

```rust
fn main() -> Result<(), StartupError> {
    let config = EngineConfig::from_env()?;
    let storage = FjallStorage::open(&config.data_dir)?;
    let engine = Engine::new(storage, config.into());
    serve(engine)
}
```

### Sorting Rules

Domain construction belongs to the engine's own types and their constructors; the root only wires concrete handles into them. Request handling belongs to routes (`zone-bin`); the root registers or starts them, never inlines them.

### Replaced Forms

A runner, pipeline, orchestrator, or step-list function calling engine operations in a hand-kept sequence is a copy of an order the construction graph (or the transaction/consuming-verb chain) already determines — name the terminal object and construct it; let dependency order fall out.

### Allowed Patterns

- one `main`/composition function: config once, concrete clients/handles instantiated, engine constructed, routes registered or served
- signal handling and graceful shutdown, living here and nowhere else

### Forbidden

- an orchestrator/pipeline/step-runner sequencing domain work by hand in the entrypoint
- a domain type or domain computation defined in the entrypoint's file
- an environment read outside config

### Halt Rule

Halt when wiring would require a domain decision or a domain type definition inside the entrypoint. Report what's missing: the terminal object is not yet named, and the table is not finished.

## Config-Once Injection

### Definition

One struct, read from the environment/CLI exactly once at startup, every field a declared scalar or secret type (`rust-values-success`), constructed by the composition root and passed down — never re-read anywhere else.

### Required Form

```rust
pub struct EngineConfig {
    data_dir: PathBuf,
    listen_port: Port,
}

impl EngineConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        // the ONE place std::env::var is called
    }
}
```

### Sorting Rules

A value that is domain state rather than an environment fact belongs on a capability handle (`rust-state-success`), never re-derived from config at read time.

### Replaced Forms

A `std::env::var` call scattered through business-logic modules is the environment read re-litigated at every call site instead of proven once at startup.

### Allowed Patterns

- one config struct per binary, constructed once, every field a declared type
- `std::env::var`/CLI-arg parsing confined entirely to the config struct's constructor

### Forbidden

- `std::env::var` (or equivalent) called anywhere outside the config struct's own constructor
- a secret held as a bare `String` instead of a type that redacts it from `Debug`/logs

### Halt Rule

Halt when an environment value has no declared type to land in. Report the value: the environment fact is not yet modeled, and the table is not finished.

## Sealed Public Door

### Definition

A host (`zone-bin`'s CLI/REPL/NATS-adapter marshal surfaces, `zone-wasm`'s envelope) consumes the engine's one sealed public contract only — a typed request in, a typed result or typed error out — and renders/routes through that envelope. It never reaches into engine internals, and never carries state of its own the engine can't account for. HTTP/gRPC is not a host pillar: at most an optional thin white-label skin over the same envelope; NATS is carriage for the Kyzo-controlled interface, not a sloppy bolt-on and not a second brain.

### Required Form

```rust
// zone-bin: consumes the envelope only
fn handle_request(engine: &Engine, request: EngineRequest) -> EngineResponse {
    engine.handle(request) // no internals imported, no result reinterpreted
}
```

`zone-bin` states it directly: a host that needs a private door is evidence the public contract is missing something; fix the contract. `zone-wasm` states the same law for the WASM envelope: one request/response shape, for every consumer, no toy build.

### Sorting Rules

If a host needs something the public contract doesn't expose, the fix is to extend the contract (a story against the engine crate), never to import an internal module from the host crate.

### Replaced Forms

A host-side cache of engine results, a shadow catalog, or "helpful" reinterpretation of a typed result before rendering it is host-side state or semantics the engine can't account for — forbidden outright in both `zone-bin` and `zone-wasm`.

### Allowed Patterns

- one envelope type (request/response), the same shape for every consumer of a given host
- rendering/routing logic only; no interpretation of the domain meaning inside a returned result

### Forbidden

- `use kyzo_core::internal_module` (or equivalent) from a host crate
- a second request/response shape for any one consumer
- host-side state (result caches, shadow catalogs) the engine doesn't know about
- `unwrap`/`expect` on any request path

### Halt Rule

Halt when a host needs data or behavior the public contract doesn't expose. Report the gap: the contract is missing a capability, and the table is not finished — the host does not route around it.
