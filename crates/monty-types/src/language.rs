use strum::{Display, EnumString, IntoStaticStr};

/// A language a Monty sandbox can run.
///
/// Two things are named by it, and they are kept apart: the *runtime* a pool
/// spawns (`monty` hosts Python and, later, Bash; `monty-js` hosts JavaScript
/// and, later, TypeScript), chosen by the language the host asks for, and the
/// *feed language*, which front end compiles one snippet among those the
/// runtime hosts. Only the runtime choice exists today; see `TODO(feed-language)`
/// for where the per-feed axis plugs in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, IntoStaticStr, serde::Serialize, serde::Deserialize,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Python, the `monty` runtime's default.
    #[strum(serialize = "python", serialize = "py")]
    Python,
    /// JavaScript, the `monty-js` runtime's default.
    #[strum(serialize = "javascript", serialize = "js")]
    JavaScript,
    /// TypeScript: JavaScript syntax with types, compiled by the `monty-js` runtime.
    #[strum(serialize = "typescript", serialize = "ts")]
    TypeScript,
    /// Bash, compiled to the Python runtime's VM.
    #[strum(serialize = "bash", serialize = "sh")]
    Bash,
}

impl Language {
    /// The language's canonical lower-case name, the first spelling `FromStr` accepts.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.into()
    }

    /// The language a source file's extension implies, if any.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension {
            "py" => Some(Self::Python),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "sh" | "bash" => Some(Self::Bash),
            _ => None,
        }
    }
}
