---
name: ontology-first-construction
description: Governs how you approach building anything. Use when the task is to design, architect, build, implement, or plan a program, system, feature, module, data model, or workflow. Trigger the moment you are about to write a numbered plan, an implementation checklist, a "first X then Y" sequence, or a build order. Also trigger on "plan how to build," "outline the steps," "design the flow," "figure out the order of operations," or "what's the approach." Apply it even when the user only describes a system and expects code: the correct first move is to model the domain, not sequence work.
---

# Ontology-First Construction

Model what exists before sequencing work.

## Method

1. Identify the domain’s authoritative values, identities, facts, choices, states, relationships, transitions, failures, and owners.
2. Give each distinct meaning one canonical type.
3. Make illegal states unconstructable. Prefer constructors that consume evidence-bearing types over constructors that accept invalid candidates and validate later.
4. Name each construct’s required inputs. These dependencies form the construction graph.
5. Derive structural build order from that graph; do not choose it manually.
6. Model runtime order that is not a construction dependency—transactions, retries, protocols, compensation, asynchronous effects—as explicit state or transition law.
7. If implementation reveals an unnamed meaning, duplicate authority, hidden dependency, or unexplained sequence, stop and extend the ontology.

Use boundary validation only to construct trusted domain types. Internally, operate on those types.

Before producing a plan or code, state:

* authoritative constructs;
* legal constructors and transitions;
* dependency graph and derived order;
* explicit temporal policies;
* unresolved design gaps.

The types define what may exist. Constructors define what must already exist. The graph supplies structural order.
