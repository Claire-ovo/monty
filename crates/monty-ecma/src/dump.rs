//! Versioned framing for serialized JavaScript sessions: one CBOR value
//! behind a `[MAGIC][DUMP_VERSION]` header, holding an idle session or a
//! feed suspended on the host. The naming contract is the Python dump's
//! (`crates/monty/src/dump_format.rs`): structs are maps keyed by field
//! name, so the schema evolves through serde, and dumps are trusted input
//! (an invalid one may fail to load or misbehave, never be unsound).

use minicbor_serde::{Deserializer, Serializer};
use serde::{Deserialize, Serialize};

use crate::repl::{JsRepl, JsSuspension};

/// Identifies a JavaScript session dump; the Python worker's dumps start differently.
pub const MAGIC: &[u8] = b"MONTYJS\0";
/// Bumped when the meaning of stored data changes (opcodes, native ids, the
/// constant layout), since serde's naming covers field additions and removals.
pub const DUMP_VERSION: u16 = 1;

const HEADER_LEN: usize = MAGIC.len() + size_of::<u16>();

/// A session to dump, borrowed: dumping is read-only.
#[derive(Clone, Copy)]
pub enum JsSessionRef<'a> {
    Idle(&'a JsRepl),
    Suspended(&'a JsSuspension),
}

/// A loaded dump: the session as it was (boxed: both hold a whole heap).
#[derive(Deserialize)]
pub enum JsDump {
    Idle(Box<JsRepl>),
    Suspended(Box<JsSuspension>),
}

/// Serializes a session into a versioned dump readable by [`JsDump::load`].
///
/// # Errors
/// Returns the serializer's message when the session cannot be encoded.
pub fn dump(session: JsSessionRef<'_>) -> Result<Vec<u8>, String> {
    /// Borrowed mirror of [`JsDump`]; serde encodes it identically.
    #[derive(Serialize)]
    enum DumpRef<'a> {
        Idle(&'a JsRepl),
        Suspended(&'a JsSuspension),
    }

    let mut bytes = Vec::with_capacity(HEADER_LEN + 1024);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&DUMP_VERSION.to_le_bytes());
    let dump = match session {
        JsSessionRef::Idle(repl) => DumpRef::Idle(repl),
        JsSessionRef::Suspended(suspension) => DumpRef::Suspended(suspension),
    };
    dump.serialize(&mut Serializer::new(&mut bytes))
        .map_err(|err| err.to_string())?;
    Ok(bytes)
}

impl JsDump {
    /// Reads a dump written by [`dump`] at this version.
    ///
    /// # Errors
    /// Names what is wrong: not a JavaScript dump, another version, or an
    /// undecodable payload.
    pub fn load(bytes: &[u8]) -> Result<Self, String> {
        let Some(header) = bytes.get(..HEADER_LEN) else {
            return Err("not a JavaScript session dump".to_owned());
        };
        if &header[..MAGIC.len()] != MAGIC {
            return Err("not a JavaScript session dump".to_owned());
        }
        let version = u16::from_le_bytes([header[MAGIC.len()], header[MAGIC.len() + 1]]);
        if version != DUMP_VERSION {
            return Err(format!(
                "dump version {version} is not supported; this build reads version {DUMP_VERSION}"
            ));
        }
        let payload = &bytes[HEADER_LEN..];
        let mut deserializer = Deserializer::new(payload);
        let mut loaded = Self::deserialize(&mut deserializer).map_err(|err| err.to_string())?;
        if deserializer.into_decoder().position() != payload.len() {
            return Err("trailing bytes after the payload".to_owned());
        }
        match &mut loaded {
            Self::Idle(repl) => repl.restore(),
            Self::Suspended(suspension) => suspension.restore(),
        }
        Ok(loaded)
    }
}
