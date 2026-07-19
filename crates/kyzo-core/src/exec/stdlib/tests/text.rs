//! Re-homed domain tables from data/tests/functions.rs.
use crate::exec::stdlib::text::*;
use kyzo_model::value::DataValue;

#[allow(dead_code)] // mid-wiring / test-only surface
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn test_str_includes() {
    assert_eq!(
        op_str_includes(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("bcd".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_str_includes(&[DataValue::Str("abcdef".into()), DataValue::Str("bd".into())]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_casings() {
    assert_eq!(
        op_lowercase(&[DataValue::Str("NAÏVE".into())]).unwrap(),
        DataValue::Str("naïve".into())
    );
    assert_eq!(
        op_uppercase(&[DataValue::Str("naïve".into())]).unwrap(),
        DataValue::Str("NAÏVE".into())
    );
}

#[test]
fn test_trim() {
    assert_eq!(
        op_trim(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str("a".into())
    );
    assert_eq!(
        op_trim_start(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str("a ".into())
    );
    assert_eq!(
        op_trim_end(&[DataValue::Str(" a ".into())]).unwrap(),
        DataValue::Str(" a".into())
    );
}

#[test]
fn test_starts_ends_with() {
    assert_eq!(
        op_starts_with(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("abc".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_starts_with(&[DataValue::Str("abcdef".into()), DataValue::Str("bc".into())]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_ends_with(&[
            DataValue::Str("abcdef".into()),
            DataValue::Str("def".into())
        ])
        .unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_ends_with(&[DataValue::Str("abcdef".into()), DataValue::Str("bc".into())]).unwrap(),
        DataValue::from(false)
    );
}

#[test]
fn test_regex() {
    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "c.e".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "c.ef$".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::from(true)
    );

    assert_eq!(
        op_regex_matches(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "c.e$".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::from(false)
    );

    assert_eq!(
        op_regex_replace(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "[be]".into()
                )
                .unwrap()
            ),
            DataValue::Str("x".into())
        ])
        .unwrap(),
        DataValue::Str("axcdef".into())
    );

    assert_eq!(
        op_regex_replace_all(&[
            DataValue::Str("abcdef".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "[be]".into()
                )
                .unwrap()
            ),
            DataValue::Str("x".into())
        ])
        .unwrap(),
        DataValue::Str("axcdxf".into())
    );
    assert_eq!(
        op_regex_extract(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "[xayef]|(GH)".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Str("a".into()),
            DataValue::Str("e".into()),
            DataValue::Str("f".into()),
            DataValue::Str("GH".into()),
        ])
    );
    assert_eq!(
        op_regex_extract_first(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "[xayef]|(GH)".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::Str("a".into()),
    );
    assert_eq!(
        op_regex_extract(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "xyz".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::List(vec![])
    );

    assert_eq!(
        op_regex_extract_first(&[
            DataValue::Str("abCDefGH".into()),
            DataValue::Regex(
                kyzo_model::value::RegexSource::validated(
                    kyzo_model::value::RegexFlags::NONE,
                    "xyz".into()
                )
                .unwrap()
            )
        ])
        .unwrap(),
        DataValue::Null
    );
}

#[test]
fn test_unicode_normalize() {
    assert_eq!(
        op_unicode_normalize(&[DataValue::Str("abc".into()), DataValue::Str("nfc".into())])
            .unwrap(),
        DataValue::Str("abc".into())
    )
}

#[test]
fn test_chars() {
    assert_eq!(
        op_from_substrings(&[op_chars(&[DataValue::Str("abc".into())]).unwrap()]).unwrap(),
        DataValue::Str("abc".into())
    )
}
