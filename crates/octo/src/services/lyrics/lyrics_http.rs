//! What the lyrics sources share: their two named HTTP clients, URL escaping, the `Retry-After`
//! delta (the shared HTTP helpers' own), and the rules for reading an answer (a property of
//! something that is not an object is a `ShapeError`, which the sources turn into a failed
//! lookup).

use std::time::Duration;

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

pub(crate) use crate::services::framework::http::retry_after_delta;

/// The client LRCLIB, NetEase and lyrics.ovh share.
pub const LYRICS_CLIENT_NAME: &str = "lyrics";

/// KuGou's own client.
pub const KUGOU_CLIENT_NAME: &str = "kugou";

/// The `lyrics` client: 8 s, Octo's user agent, and no cookies. NetEase answers a search that
/// carries the cookie its first answer set with unrelated popular songs, so with the default
/// handler every search after the first missed. (reqwest keeps no cookies unless asked.)
pub fn lyrics_http_client() -> reqwest::Client {
    client(Duration::from_secs(8))
}

/// The `kugou` client. KuGou's API is unofficial: a short timeout of its own, so a slow or dead
/// KuGou costs a lookup a few seconds at most.
pub fn kugou_http_client() -> reqwest::Client {
    client(Duration::from_secs(6))
}

fn client(timeout: Duration) -> reqwest::Client {
    // No decompression.
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(octo_core::common::octo_user_agent::value())
        .no_gzip()
        .no_deflate()
        .build()
        .expect("a plain HTTP client builds")
}

/// Percent-escapes everything but the RFC 3986 unreserved characters.
const DATA_STRING: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub(crate) fn escape_data_string(text: &str) -> String {
    utf8_percent_encode(text, DATA_STRING).to_string()
}

/// An integer, with white space either side and a leading sign allowed.
pub(crate) fn try_parse_long(text: &str) -> Option<i64> {
    text.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' '))
        .parse::<i64>()
        .ok()
}

/// Base64: standard alphabet, padding required, white space ignored, and unused trailing bits not
/// checked.
pub(crate) fn from_base64(text: &str) -> Option<Vec<u8>> {
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let compact: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect();
    LENIENT.decode(compact).ok()
}

/// An answer of a shape the source did not expect: a property asked of something that is not an
/// object, a string of something that is not one, or a whole number of a fraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShapeError;

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the answer is not of the expected shape")
    }
}

/// The property `name`: an error when the element is not an object. A repeated name gives the
/// last value.
pub(crate) fn prop<'a>(element: &'a Value, name: &str) -> Result<Option<&'a Value>, ShapeError> {
    match element {
        Value::Object(map) => Ok(map.get(name)),
        _ => Err(ShapeError),
    }
}

/// The property when it is a string, else None.
pub(crate) fn str_prop<'a>(element: &'a Value, name: &str) -> Result<Option<&'a str>, ShapeError> {
    Ok(prop(element, name)?.and_then(Value::as_str))
}

/// The string: None for a JSON null, an error for anything but a string.
pub(crate) fn get_string(value: &Value) -> Result<Option<&str>, ShapeError> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text)),
        _ => Err(ShapeError),
    }
}

/// A number as an i64: an error when it is not a whole number in range.
pub(crate) fn get_i64(value: &Value) -> Result<i64, ShapeError> {
    try_get_i64(value)?.ok_or(ShapeError)
}

/// A number as an i64: None for a number that is not a whole number in range, and an error for
/// anything that is not a number.
pub(crate) fn try_get_i64(value: &Value) -> Result<Option<i64>, ShapeError> {
    match value {
        Value::Number(number) => Ok(number.as_i64()),
        _ => Err(ShapeError),
    }
}

/// A number as an i32, as [`try_get_i64`] reads it.
pub(crate) fn try_get_i32(value: &Value) -> Result<Option<i32>, ShapeError> {
    Ok(try_get_i64(value)?.and_then(|n| i32::try_from(n).ok()))
}

/// Rounded half to even, then cast to an i32.
pub(crate) fn round_to_int(value: f64) -> i32 {
    value.round_ties_even() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_data_string_keeps_only_the_unreserved_characters() {
        assert_eq!(
            escape_data_string("$uicideboy$ - $UICIDE"),
            "%24uicideboy%24%20-%20%24UICIDE"
        );
        assert_eq!(escape_data_string("a.b_c~d"), "a.b_c~d");
        assert_eq!(escape_data_string("宇多田"), "%E5%AE%87%E5%A4%9A%E7%94%B0");
    }

    #[test]
    fn numbers_and_base64_read_leniently() {
        assert_eq!(try_parse_long(" +42 "), Some(42));
        assert_eq!(try_parse_long("4 2"), None);
        assert_eq!(try_parse_long("1.5"), None);
        assert_eq!(from_base64("aGk=\n"), Some(b"hi".to_vec()));
        assert_eq!(from_base64("aGk"), None);
        assert_eq!(round_to_int(2.5), 2);
        assert_eq!(round_to_int(3.5), 4);
    }

    #[test]
    fn a_property_of_something_that_is_not_an_object_is_a_shape_error() {
        let json: Value = serde_json::from_str(r#"{"a":"x","n":1.5,"i":7,"z":null}"#).expect("json");
        assert_eq!(str_prop(&json, "a"), Ok(Some("x")));
        assert_eq!(str_prop(&json, "n"), Ok(None));
        assert_eq!(prop(&Value::Array(vec![]), "a"), Err(ShapeError));
        assert_eq!(try_get_i64(&json["n"]), Ok(None));
        assert_eq!(get_i64(&json["i"]), Ok(7));
        assert_eq!(get_i64(&json["n"]), Err(ShapeError));
        assert_eq!(try_get_i64(&json["a"]), Err(ShapeError));
        assert_eq!(get_string(&json["z"]), Ok(None));
        assert_eq!(get_string(&json["i"]), Err(ShapeError));
    }
}
