//! metric.rs — stdlib kernel (move_plan).

use miette::{Result, bail};

use kyzo_model::data_value_any;
use kyzo_model::value::DataValue;

use crate::exec::stdlib::errors::{DomainError, no_nan, no_nan_vec};

pub(crate) fn op_cos_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'cos_dist' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            let a_norm: f64 = sa.iter().map(|x| x * x).sum();
            let b_norm: f64 = sb.iter().map(|x| x * x).sum();
            if a_norm == 0.0 || b_norm == 0.0 {
                bail!(DomainError {
                    op: "cos_dist".into()
                });
            }
            let dot: f64 = sa.iter().zip(sb.iter()).map(|(x, y)| *x * *y).sum();
            no_nan("cos_dist", 1. - dot / (a_norm * b_norm).sqrt())
        }
        _ => bail!("'cos_dist' requires two vectors"),
    }
}

pub(crate) fn op_ip_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'ip_dist' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            let dot: f64 = sa.iter().zip(sb.iter()).map(|(x, y)| *x * *y).sum();
            Ok(DataValue::from(1. - dot))
        }
        _ => bail!("'ip_dist' requires two vectors"),
    }
}

pub(crate) fn op_l2_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'l2_dist' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            let d: f64 = sa
                .iter()
                .zip(sb.iter())
                .map(|(x, y)| (*x - *y) * (*x - *y))
                .sum();
            Ok(DataValue::from(d))
        }
        _ => bail!("'l2_dist' requires two vectors"),
    }
}

pub(crate) fn op_l2_normalize(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    match a {
        DataValue::Vector(a) => {
            let s = a.to_f64s();
            let norm = s.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm == 0.0 {
                bail!(DomainError {
                    op: "l2_normalize".into()
                });
            }
            no_nan_vec("l2_normalize", s.iter().map(|x| x / norm).collect())
        }
        data_value_any!() => bail!("'l2_normalize' requires a vector"),
    }
}
