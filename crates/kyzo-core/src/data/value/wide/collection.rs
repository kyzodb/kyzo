/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `List` / `Set`: the collection faces ARE the canonical sequence
//! grammar (see [`super::super::canonical`]) — elements' self-terminating
//! encodings concatenated under a terminator, with Set canonicalized to
//! the sorted, deduplicated element sequence at encode and REFUSED (not
//! repaired) at decode if non-canonical. There is no separate wide
//! encoding: a big collection goes out of line as the same canonical
//! bytes behind a `Code`, by the cell's residency threshold — residency
//! is never identity.

#[cfg(test)]
mod tests {
    use crate::data::value::data_value_any;
    use super::super::super::DataValue;
    use super::super::super::canonical::{Datum, decode, encode};
    use super::super::super::number::Num;

    #[test]
    fn nested_collection_identity_round_trips() {
        let inner = [Datum::Num(Num::int(2)), Datum::Num(Num::int(1))];
        let outer = [Datum::Set(&inner), Datum::List(&inner)];
        let enc = encode(Datum::List(&outer));
        let back = decode(enc.as_bytes()).expect("round-trip");
        match &back {
            DataValue::List(items) => {
                // The set canonicalized to sorted order; the list kept
                // writing order.
                assert!(matches!(&items[0], DataValue::Set(s)
                    if matches!(s.iter().next(), Some(DataValue::Num(n)) if *n == Num::int(1))));
                assert!(matches!(&items[1], DataValue::List(l)
                    if matches!(&l[0], DataValue::Num(n) if *n == Num::int(2))));
            }
            other @ (data_value_any!()) => panic!("wrong shape: {other:?}"),
        }
    }
}
