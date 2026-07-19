//! IO fixed rules that cross into foreign file formats.

pub(crate) mod constant;
pub(crate) mod csv;
pub(crate) mod jlines;

pub(crate) use constant::Constant;
pub(crate) use csv::CsvReader;
pub(crate) use jlines::JsonReader;
