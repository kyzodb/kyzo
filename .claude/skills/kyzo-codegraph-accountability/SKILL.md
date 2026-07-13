---
name: kyzo-codegraph-accountability
description: "Use when the purity number moved (or didn't) and you need to know exactly why, when a claim needs cross-examining down to its evidence, or when you need history — what the graph believed at an earlier instant, what changed between two rounds, which law took which measurement. The forensics layer: nothing here is inferred, everything is read off records."
---

# Accountability: cross-examine the number

The purity score is a conclusion, and every conclusion in this system decomposes into records.
Never speculate about why the number is what it is — read it. The chain runs:
**score → zone terms → claims → rule version + evidence → history.**

## Why is the number what it is?

```
?[at, score, bad, constructs, doctrine_digest] := *purity{at, score, bad, constructs, doctrine_digest}
:order -at :limit 5                                   # the recent measurements, instruments named
```

**First check: did the digest change between rows?** If yes, the LAW moved — the rounds are
different instruments and their scores must not be compared as if the code moved (that is what
the `≠` arrow means). Same digest → the code moved; go find where:

```
?[zone, eligible, tainted] := *purity_zone{at, zone, eligible, tainted}, at == $t1
?[zone, eligible, tainted] := *purity_zone{at, zone, eligible, tainted}, at == $t2
```

The zone whose `tainted` or `eligible` shifted is your culprit. Then name the constructs:

```
?[file, symbol, concept, rule] :=
    *claim{subject, concept, rule, zone, standing, tier, valid_to}, zone == $z,
    standing == 'affirmed', valid_to == "",
    *concept{id: concept, polarity, valid_to: kvt}, polarity == 'bad', kvt == "",
    *ast{cid: subject, file, symbol, valid_to: avt}, avt == ""
```

(That polarity join matters: the score counts affirmed **bad-polarity** claims over live
non-test constructs, each construct once. `good` claims are recorded but never scored.)

## Cross-examine one claim

Every claim answers the full accountability question set — read all of it, including closed
versions (drop the liveness filter deliberately):

```
?[concept, rule, rule_from, evidence_node, evidence_sha, zone, tier, standing,
  authority, reason, status, valid_from, valid_to] :=
    *claim{id, concept, rule, rule_from, evidence_node, evidence_sha, zone, tier,
           standing, authority, reason, status, valid_from, valid_to}, id == $claim
```

- `rule_from` pins the rule VERSION — compare against the live rule to see if the law has
  since moved from under it.
- `evidence_sha` pins the content the claim was true OF; if the node's live `content_sha`
  differs, the code has changed since (the claim will have been superseded — check `status`).
- Judged claims: `authority == 'model'`, `reason` carries the judge's words, and WHO settled
  it is on the adjudication event, not the claim: `*adjudication{claim, by, reason, at}`.
- The rule's whole examination history: `*examination{rule: $r, subject, privation, reason}` —
  the assent rate is the rule's real-world precision.

## Time travel — the graph at any instant

Timestamps are ISO strings; comparison is lexicographic. The as-of pattern works on every
versioned relation:

```
?[file, symbol, concept] :=
    *claim{subject, concept, standing, valid_from, valid_to}, standing == 'affirmed',
    valid_from <= $t, (valid_to == "" || valid_to > $t),
    *ast{cid: subject, file, symbol, valid_from: af, valid_to: at_},
    af <= $t, (at_ == "" || at_ > $t)
```

That is "the affirmed findings as things stood at `$t`" — regressions, audits, and "was this
known before the incident?" are all this one shape. `status` distinguishes how versions
closed: `superseded` (newer truth replaced it) vs `invalidated` (the file left the tree).

## What changed between two rounds?

Construct-level churn is recorded as events, not recomputed:

```
?[kind, file, symbol, at] := *change_event{kind, file, symbol, at}, at > $since
:order at
```

`from_sha → to_sha` on `modified` events pins exactly which body change happened. Join `cid`
back to `*claim{subject}` to see which findings a change created or killed.

## The coverage companions — what the number does NOT know yet

Beside every measurement: `suspects` (flag-gate questions awaiting a bit), `examined` (already
answered at this content), `debt`/`debt_covered` (macro surface without/with expansion), and
the update report's `stale_generation` (records parsed by an older tool). A high score with
low `examined` or high `debt` is an honest number with a named blind spot — report both
halves, never just the score.

## Discipline

Read history freely; never write it. If a record looks wrong, the answer is a doctrine change,
a re-round, or an adjudication through the doors — never a hand edit to a relation. The value
of every query above rests on no one having ever done that.
