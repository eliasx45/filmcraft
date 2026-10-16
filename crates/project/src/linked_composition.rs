//! Saved EffectCraft references, independent of the process used to render them.

use serde::{Deserialize, Serialize};

/// One visual-only composition behind a FilmCraft media item. Timeline positions and source
/// trims remain on the timeline clips. Save As never changes this reference implicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedComposition {
    /// Absolute source path at link time; moving the source requires explicit relinking.
    pub project_path: String,
    /// Stable numeric item id inside that EffectCraft project (not a composition name).
    pub composition_id: u64,
    /// A baked item keeps its source reference but no longer refreshes automatically.
    #[serde(default)]
    pub baked: bool,
}

impl LinkedComposition {
    /// Validate at command/load boundaries before opening files or starting a renderer.
    pub fn validate(&self) -> Result<(), String> {
        if self.project_path.trim().is_empty() || self.project_path.contains('\0') || self.project_path.len() > 32_768 {
            return Err("EffectCraft project path is empty, too long, or contains a NUL".into());
        }
        if self.composition_id == 0 {
            return Err("EffectCraft composition id must be nonzero".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_references_are_errors() {
        for path in [String::new(), "  ".into(), "a\0b".into(), "x".repeat(32_769)] {
            assert!(LinkedComposition { project_path: path, composition_id: 1, baked: false }.validate().is_err());
        }
        assert!(LinkedComposition { project_path: "titles.ecproj".into(), composition_id: 0, baked: false }.validate().is_err());
    }
}
