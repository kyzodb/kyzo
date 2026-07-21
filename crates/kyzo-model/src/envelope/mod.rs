//! Wire envelopes: values crossing process / host boundaries.

pub mod arrow;
pub mod json;

pub use json::{
    JsonData, JsonValue, datavalue_to_json, json_from_serde, json_to_datavalue, serde_from_json,
};
