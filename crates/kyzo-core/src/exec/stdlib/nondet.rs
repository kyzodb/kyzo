//! nondet.rs — stdlib kernel (move_plan).
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::{Div, Rem};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use itertools::Itertools;
use jiff::tz::{Offset, TimeZone};
use miette::{Diagnostic, IntoDiagnostic, Result, bail, ensure, miette};
use rand::prelude::*;
use serde_json::{Value, json};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use uuid::v1::Timestamp;

use kyzo_model::data_value_any;
use kyzo_model::value::{
    Bound, DataValue, Interval, Json, Num, NumRepr, NumericOrd, RegexFlags, RegexSource, Validity,
    ValidityTs, Vector,
};
use kyzo_model::{json_from_serde, serde_from_json};
use serde_json::Value as JsonValue;

use crate::exec::stdlib::convert::vec_element_type;
use crate::exec::stdlib::errors::{
    DivisionByZero, DomainError, IntegerOverflow, StdlibRefuse, TimestampFormatRefused,
    VecOpEmptyArgs, no_nan, no_nan_vec, result_has_nan, vec_value,
};
use kyzo_model::schema::VecElementType;

pub(crate) fn op_now(_args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(unix_now()?.as_secs_f64()))
}

pub(crate) fn op_rand_bernoulli(args: &[DataValue]) -> Result<DataValue> {
    let prob = match &args[0] {
        DataValue::Num(n) => {
            let f = n.to_f64();
            ensure!(
                (0. ..=1.).contains(&f),
                "'rand_bernoulli' requires number between 0. and 1."
            );
            f
        }
        data_value_any!() => bail!("'rand_bernoulli' requires number between 0. and 1."),
    };
    Ok(DataValue::from(rand::rng().random_bool(prob)))
}

pub(crate) fn op_rand_choose(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(l) => Ok(l
            .choose(&mut rand::rng())
            .cloned()
            .unwrap_or(DataValue::Null)),
        DataValue::Set(l) => Ok(l
            .iter()
            .collect_vec()
            .choose(&mut rand::rng())
            .cloned()
            .cloned()
            .unwrap_or(DataValue::Null)),
        data_value_any!() => bail!("'rand_choice' requires lists"),
    }
}

pub(crate) fn op_rand_float(_args: &[DataValue]) -> Result<DataValue> {
    Ok(rand::rng().random::<f64>().into())
}

pub(crate) fn op_rand_int(args: &[DataValue]) -> Result<DataValue> {
    let lower = &args[0]
        .get_int()
        .ok_or_else(|| miette!("'rand_int' requires integers"))?;
    let upper = &args[1]
        .get_int()
        .ok_or_else(|| miette!("'rand_int' requires integers"))?;
    // Checked here because rand 0.9's `random_range` panics on an empty
    // range, and an op body must never panic on user input.
    ensure!(
        lower <= upper,
        "'rand_int' requires a lower bound not greater than the upper bound"
    );
    Ok(rand::rng().random_range(*lower..=*upper).into())
}

pub(crate) fn op_rand_uuid_v1(_args: &[DataValue]) -> Result<DataValue> {
    let mut rng = rand::rng();
    let uuid_ctx = uuid::ContextV1::new(rng.random());
    let ts = {
        let since_epoch = unix_now()?;
        Timestamp::from_unix(uuid_ctx, since_epoch.as_secs(), since_epoch.subsec_nanos())
    };
    let mut rand_vals = [0u8; 6];
    rng.fill(&mut rand_vals);
    let id = uuid::Uuid::new_v1(ts, &rand_vals);
    Ok(DataValue::uuid(id))
}

pub(crate) fn op_rand_uuid_v4(_args: &[DataValue]) -> Result<DataValue> {
    let id = uuid::Uuid::new_v4();
    Ok(DataValue::uuid(id))
}

pub(crate) fn op_rand_vec(args: &[DataValue]) -> Result<DataValue> {
    let len_i = args[0]
        .get_int()
        .ok_or_else(|| miette!("'rand_vec' requires an integer"))?;
    let len = usize::try_from(len_i)
        .map_err(|_| miette!("'rand_vec' length must be non-negative, got {len_i}"))?;
    let t = vec_element_type(args.get(1), "rand_vec")?;

    let mut rng = rand::rng();
    let components: Vec<f64> = (0..len)
        .map(|_| match t {
            VecElementType::F32 => rng.random::<f64>() as f32 as f64,
            VecElementType::F64 => rng.random::<f64>(),
        })
        .collect();
    Ok(DataValue::Vector(vec_value(components)?))
}

/// The host clock as a duration since the Unix epoch.
///
/// Policy (documented choice): a clock reading before 1970 is an **error**,
/// not saturation — a time-travel database whose host clock is decades wrong
/// should refuse loudly rather than silently write validity at the epoch.
/// The CozoDB original unwrapped and aborted the process.
fn unix_now() -> Result<Duration> {
    #[derive(Debug, Error, Diagnostic)]
    #[error("The system clock reads earlier than the Unix epoch")]
    #[diagnostic(code(eval::clock_before_epoch))]
    #[diagnostic(help("Fix the host clock; timestamps are seconds since 1970-01-01T00:00:00Z"))]
    struct ClockBeforeEpochError;

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ClockBeforeEpochError.into())
}
