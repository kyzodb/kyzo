---
name: ontology-first-construction
description: Governs how you approach building anything. Use when the task is to design, architect, build, implement, or plan a program, system, feature, module, data model, or workflow. Trigger the moment you are about to write a numbered plan, an implementation checklist, a "first X then Y" sequence, or a build order. Also trigger on "plan how to build," "outline the steps," "design the flow," "figure out the order of operations," or "what's the approach." Apply it even when the user only describes a system and expects code: the correct first move is to model the domain, not sequence work.
---

# Ontology-First Construction

Describe the world, not the behavior. Behavior is entailed by the world model. Once the domain is fully typed, execution order is not a decision: it is the topological order of the dependency graph the types form. The types are the plan; there is no plan beside them.

## Ask what exists

The question is "what exists?", never "what steps happen in what order?" Enumerate the domain exhaustively: every value, fact, choice, state, and edge. Each is a thing that exists, so each has a type. Investigate with the tools available until nothing lacks an entry. An incomplete enumeration is the source of every step you are later forced to write by hand.

## One true type per thing

Give each thing its one canonical type, with its constraints carried inside its construction. A value that cannot be built in an invalid form cannot exist in one, so invariants belong in constructors, not in checks scattered downstream. Illegal states are unrepresentable because no constructor produces them. If a type permits a combination the domain forbids, the type is wrong: redesign it so that combination cannot be constructed.

## Order is derived, not chosen

A composite cannot be constructed before its constituents exist. This is a fact about construction, not a policy. Once every construct is typed and every constructor names what it consumes, the dependency structure is fixed and sequence follows from it. Read construction order off the graph as its topological order. Derive it; never pick it.

## Steps are a symptom

You write steps only when the world is under-described. A hand-sequenced plan confesses a missing type: the sequence stands in for a dependency the model failed to encode. When you catch yourself ordering operations by hand, stop, find the missing type, and put the dependency inside a constructor; the order returns entailed. The diagnostic: whenever you write "first X, then Y" for any reason other than reading order off the graph, a type is missing, not a step. Model the type and the step disappears.

Model until nothing is left to decide. The only remaining work is constructing what the model already proved could exist, and that construction is the code.

## Procedure

1. Enumerate every value, fact, choice, state, and edge in the domain, exhaustively.
2. Assign one canonical type per entry. Where you were about to allow an invalid combination, redesign the type so it cannot be constructed.
3. Move every invariant into a constructor as a precondition. Nothing valid should need a downstream check to stay valid.
4. Name each composite's constituents in its constructor, making the dependency graph explicit.
5. Derive construction order as the graph's topological order.
6. Construct only the code that builds each typed construct. If a line is sequencing logic the graph did not entail, halt: a type is missing. Return to step 1 for that region, then resume.

## Worked shape

Step-plan: "1. Validate the order. 2. Reserve inventory. 3. Charge the card. 4. Create the shipment." Four numbers, four confessions. The domain's real content: a ReservedOrder cannot construct from an Order without available Inventory; a Charge cannot construct without a ReservedOrder; a Shipment cannot construct without a completed Charge. Put those preconditions in the constructors and the sequence is entailed. No one ordered the steps; the constructors did.