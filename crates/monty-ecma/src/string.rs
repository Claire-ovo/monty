//! JavaScript strings: immutable sequences of UTF-16 code units.
//!
//! A string is stored as Latin-1 bytes when every code unit fits in one, and
//! as UTF-16 otherwise, so the common ASCII case costs one byte per character
//! while lone surrogates (which no Rust `str` can hold) stay representable.
//! Equality, hashing and ordering are by code unit, as the language defines.

use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

/// An immutable JavaScript string.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum JsString {
    /// Every code unit is below 256.
    #[serde(rename = "L")]
    Latin1(#[serde(with = "serde_bytes_compat")] Box<[u8]>),
    /// At least one code unit is 256 or above.
    #[serde(rename = "U")]
    Utf16(Box<[u16]>),
}

impl JsString {
    /// The empty string.
    pub fn empty() -> Self {
        Self::Latin1(Box::new([]))
    }

    /// Builds a string from UTF-16 code units, compacting to Latin-1 when possible.
    pub fn from_units(units: &[u16]) -> Self {
        if units.iter().all(|unit| *unit < 0x100) {
            // every unit fits a byte, which is the lossless path here
            #[expect(clippy::cast_possible_truncation, reason = "checked above")]
            let bytes: Box<[u8]> = units.iter().map(|unit| *unit as u8).collect();
            Self::Latin1(bytes)
        } else {
            Self::Utf16(units.into())
        }
    }

    /// Builds a string from Rust text (which never holds a lone surrogate).
    pub fn from_str(text: &str) -> Self {
        if text.is_ascii() {
            Self::Latin1(text.as_bytes().into())
        } else if text.chars().all(|c| (c as u32) < 0x100) {
            let bytes: Box<[u8]> = text.chars().map(|c| c as u8).collect();
            Self::Latin1(bytes)
        } else {
            Self::Utf16(text.encode_utf16().collect())
        }
    }

    /// Builds a string from code units yielded by an iterator (an oxc literal).
    pub fn from_unit_iter(units: impl Iterator<Item = u16>) -> Self {
        let units: Vec<u16> = units.collect();
        Self::from_units(&units)
    }

    /// The number of code units: JavaScript's `length`.
    pub fn len(&self) -> usize {
        match self {
            Self::Latin1(bytes) => bytes.len(),
            Self::Utf16(units) => units.len(),
        }
    }

    /// Whether the string has no code units.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The code unit at `index`, if in range.
    pub fn unit_at(&self, index: usize) -> Option<u16> {
        match self {
            Self::Latin1(bytes) => bytes.get(index).map(|b| u16::from(*b)),
            Self::Utf16(units) => units.get(index).copied(),
        }
    }

    /// Iterates the code units.
    pub fn units(&self) -> impl Iterator<Item = u16> + '_ {
        let (latin1, utf16): (Option<&[u8]>, Option<&[u16]>) = match self {
            Self::Latin1(bytes) => (Some(bytes), None),
            Self::Utf16(units) => (None, Some(units)),
        };
        latin1
            .into_iter()
            .flatten()
            .map(|b| u16::from(*b))
            .chain(utf16.into_iter().flatten().copied())
    }

    /// The code units as an owned vector.
    pub fn to_units(&self) -> Vec<u16> {
        self.units().collect()
    }

    /// The substring of code units `start..end` (both clamped to the length).
    pub fn slice(&self, start: usize, end: usize) -> Self {
        let end = end.min(self.len());
        let start = start.min(end);
        match self {
            Self::Latin1(bytes) => Self::Latin1(bytes[start..end].into()),
            Self::Utf16(units) => Self::from_units(&units[start..end]),
        }
    }

    /// Whether the string is a canonical array index: a decimal integer below
    /// 2³² − 1 with no leading zero (`"0"` included, `"01"` and `"-1"` excluded).
    pub fn as_array_index(&self) -> Option<u32> {
        let Self::Latin1(bytes) = self else {
            return None;
        };
        if bytes.is_empty() || bytes.len() > 10 || (bytes.len() > 1 && bytes[0] == b'0') {
            return None;
        }
        let mut value: u64 = 0;
        for byte in bytes {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + u64::from(byte - b'0');
        }
        // 2³² − 1 is the array length limit, not a valid index
        u32::try_from(value).ok().filter(|index| *index != u32::MAX)
    }

    /// Converts to Rust text, replacing lone surrogates with U+FFFD.
    pub fn to_string_lossy(&self) -> String {
        match self {
            Self::Latin1(bytes) => bytes.iter().map(|b| char::from(*b)).collect(),
            Self::Utf16(units) => String::from_utf16_lossy(units),
        }
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Latin1(a), Self::Latin1(b)) => a == b,
            (Self::Utf16(a), Self::Utf16(b)) => a == b,
            // `from_units` compacts, so a Latin-1 and a UTF-16 string never hold the same units
            _ => false,
        }
    }
}

impl Eq for JsString {}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // hash code units so the two representations of equal content agree
        state.write_usize(self.len());
        for unit in self.units() {
            state.write_u16(unit);
        }
    }
}

impl PartialOrd for JsString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsString {
    /// Code-unit order, as `<` on strings is defined.
    fn cmp(&self, other: &Self) -> Ordering {
        self.units().cmp(other.units())
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self::from_str(text)
    }
}

/// Serializes `Box<[u8]>` as one byte string rather than one integer per byte.
mod serde_bytes_compat {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Box<[u8]>, D::Error> {
        let bytes: Vec<u8> = serde_bytes::ByteBuf::deserialize(deserializer)?.into_vec();
        Ok(bytes.into_boxed_slice())
    }
}
