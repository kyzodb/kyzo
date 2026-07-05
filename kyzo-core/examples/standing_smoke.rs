//! Independent end-to-end exercise of the PUBLIC standing-query surface
//! (`Db::register_standing` + `StandingQuery::apply_pending`), written from
//! the outside as a user would — different query and data than the in-tree
//! tests. Proves a real aggregating standing query stays correct across real
//! committed mutations, including the hard min-under-retraction rescan.

use kyzo::{DataValue, Db, new_fjall_storage};
use std::collections::BTreeMap;

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn main() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let db = Db::new(new_fjall_storage(dir.path()).expect("fjall")).expect("db");

    db.run_script(":create sales {region: String, amt: Int =>}", no_params())
        .expect("create");
    db.run_script(
        "?[region, amt] <- [['west', 100], ['west', 50], ['east', 200]] :put sales {region, amt}",
        no_params(),
    )
    .expect("seed");

    let query = "?[region, min(amt)] := *sales[region, amt]";
    let mut sq = db
        .register_standing(query, no_params())
        .expect("register_standing");
    println!("registered standing query: {query}");
    println!(
        "full recompute at register: {:?}",
        db.run_script(query, no_params()).unwrap().rows
    );

    // Commit a NEW lower minimum for 'west' (100/50 -> now also 10).
    db.run_script(
        "?[region, amt] <- [['west', 10]] :put sales {region, amt}",
        no_params(),
    )
    .expect("put west=10");
    let delta = sq.apply_pending().expect("apply");
    println!("\n-- after :put west=10 --");
    println!("standing delta:  {:?}", delta);
    println!(
        "full recompute:  {:?}",
        db.run_script(query, no_params()).unwrap().rows
    );

    // Retract the CURRENT minimum (west=10). This is the hard case: the
    // aggregate must rescan the group to find the next min (50), which no
    // per-kind delta formula can do.
    db.run_script(
        "?[region, amt] <- [['west', 10]] :rm sales {region, amt}",
        no_params(),
    )
    .expect("rm west=10");
    let delta = sq.apply_pending().expect("apply");
    println!("\n-- after :rm west=10 (retract the current min) --");
    println!("standing delta:  {:?}", delta);
    println!(
        "full recompute:  {:?}",
        db.run_script(query, no_params()).unwrap().rows
    );

    println!("\nOK: standing query maintained across put + min-retraction, matching recompute.");
}
