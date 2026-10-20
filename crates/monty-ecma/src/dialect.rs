//! Which syntax a session's feeds are parsed with.
//!
//! The script name decides, as it does for Node and `tsc`: `.ts` feeds are
//! TypeScript, `.tsx` TypeScript with JSX, `.jsx` JavaScript with JSX, and
//! anything else plain JavaScript. The dialect is fixed when the session is
//! created, so every feed of a session speaks the same language.

use oxc_span::SourceType;
use serde::{Deserialize, Serialize};

/// The syntax switches a feed is parsed with; both off is plain JavaScript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Dialect {
    /// TypeScript: types are erased before compilation (`typescript.rs` rejects
    /// the constructs that would need emission).
    pub typescript: bool,
    /// JSX: elements lower to calls of the realm's `jsx` function.
    pub jsx: bool,
}

impl Dialect {
    /// The dialect a script name's extension implies.
    pub fn from_script_name(name: &str) -> Self {
        let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        match extension {
            "ts" | "mts" | "cts" => Self {
                typescript: true,
                jsx: false,
            },
            "tsx" => Self {
                typescript: true,
                jsx: true,
            },
            "jsx" => Self {
                typescript: false,
                jsx: true,
            },
            _ => Self::default(),
        }
    }

    /// Whether the parsed program needs oxc's transformer before compilation.
    pub fn needs_transform(self) -> bool {
        self.typescript || self.jsx
    }

    /// oxc's view of the dialect: always an ES module, so feeds are strict.
    pub fn source_type(self) -> SourceType {
        SourceType::mjs().with_typescript(self.typescript).with_jsx(self.jsx)
    }
}
