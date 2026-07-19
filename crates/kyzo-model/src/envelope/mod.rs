//! Wire envelopes: values crossing process / host boundaries.

pub mod arrow;
pub mod json;

pub use json::{JsonData, JsonValue, json_from_serde, json_to_datavalue, serde_from_json};
