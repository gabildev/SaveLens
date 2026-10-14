//! Embedded save-path rule database (Ludusavi-style path templates).
//!
//! The builtin rules live in `data/save_rules.toml` and are compiled into the binary. User rule
//! files ([`crate::DiscoveryConfig::extra_rules`]) use the same format; a user rule with the
//! same `id` as a builtin one replaces it, other user rules are appended.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::paths::PathOs;
use crate::paths::{make_location, validate_template, PathContext};
use crate::{DiscoveryError, LocationSource, SaveLocation};

/// The builtin rule database source.
pub const BUILTIN_RULES: &str = include_str!("../data/save_rules.toml");

/// One path of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulePath {
    /// Path template, e.g. `<winAppData>/StardewValley/Saves`.
    pub path: String,
    /// Which platform conventions the path applies to.
    #[serde(default = "default_os")]
    pub os: PathOs,
    /// Overrides the rule-level patterns for this path.
    #[serde(default)]
    pub patterns: Option<Vec<String>>,
}

fn default_os() -> PathOs {
    PathOs::Any
}

fn default_patterns() -> Vec<String> {
    vec!["*".to_string()]
}

/// A save rule for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRule {
    /// Unique identifier (`stardew-valley`).
    pub id: String,
    /// Display name; also used for matching games without an app id.
    pub name: String,
    /// Steam app ids this rule applies to.
    #[serde(default)]
    pub app_ids: Vec<u32>,
    /// File patterns (globs relative to the save root).
    #[serde(default = "default_patterns")]
    pub patterns: Vec<String>,
    /// Candidate save directories.
    pub paths: Vec<RulePath>,
}

impl SaveRule {
    /// Does this rule apply to a game with this app id / name?
    pub fn matches(&self, app_id: Option<u32>, name: &str) -> bool {
        match app_id {
            Some(id) if self.app_ids.contains(&id) => true,
            _ => self.name.eq_ignore_ascii_case(name.trim()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[serde(default, rename = "game")]
    games: Vec<SaveRule>,
}

/// An ordered collection of save rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    rules: Vec<SaveRule>,
}

impl RuleSet {
    /// The embedded database.
    pub fn builtin() -> Result<Self, DiscoveryError> {
        Self::from_toml(BUILTIN_RULES, "builtin")
    }

    /// Parses and validates a rule file. `origin` names the source in error messages.
    pub fn from_toml(text: &str, origin: &str) -> Result<Self, DiscoveryError> {
        let rules_err = |message: String| DiscoveryError::Rules { origin: origin.to_string(), message };
        let file: RuleFile = toml::from_str(text).map_err(|e| rules_err(e.to_string()))?;
        let mut ids = std::collections::BTreeSet::new();
        for rule in &file.games {
            if rule.id.trim().is_empty() {
                return Err(rules_err(format!("rule `{}` has an empty id", rule.name)));
            }
            if !ids.insert(rule.id.clone()) {
                return Err(rules_err(format!("duplicate rule id `{}`", rule.id)));
            }
            if rule.paths.is_empty() {
                return Err(rules_err(format!("rule `{}` has no paths", rule.id)));
            }
            for p in &rule.paths {
                validate_template(&p.path, &[]).map_err(|m| rules_err(format!("rule `{}`: {m}", rule.id)))?;
                for pat in p.patterns.iter().flatten().chain(&rule.patterns) {
                    glob::Pattern::new(pat)
                        .map_err(|e| rules_err(format!("rule `{}`: invalid pattern `{pat}`: {e}", rule.id)))?;
                }
            }
        }
        Ok(RuleSet { rules: file.games })
    }

    /// Reads and validates a user rule file.
    pub fn from_file(path: &Path) -> Result<Self, DiscoveryError> {
        let text = std::fs::read_to_string(path).map_err(|e| DiscoveryError::io(path, e))?;
        Self::from_toml(&text, &path.display().to_string())
    }

    /// Builtin rules merged with every file of `extra` (in order).
    pub fn load(extra: &[PathBuf]) -> Result<Self, DiscoveryError> {
        let mut set = Self::builtin()?;
        for path in extra {
            set.merge(Self::from_file(path)?);
        }
        Ok(set)
    }

    /// Merges `other` into `self`: same id replaces, new ids are appended.
    pub fn merge(&mut self, other: RuleSet) {
        for rule in other.rules {
            match self.rules.iter_mut().find(|r| r.id == rule.id) {
                Some(existing) => *existing = rule,
                None => self.rules.push(rule),
            }
        }
    }

    /// All rules.
    pub fn rules(&self) -> &[SaveRule] {
        &self.rules
    }

    /// The rule with this id.
    pub fn get(&self, id: &str) -> Option<&SaveRule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// Rules applying to a game.
    pub fn matching<'a>(&'a self, app_id: Option<u32>, name: &'a str) -> impl Iterator<Item = &'a SaveRule> + 'a {
        self.rules.iter().filter(move |r| r.matches(app_id, name))
    }
}

/// Resolves the paths of `rule` for one game into existing save locations.
pub(crate) fn resolve_rule(rule: &SaveRule, ctx: &PathContext) -> Result<Vec<SaveLocation>, DiscoveryError> {
    let mut out: Vec<SaveLocation> = Vec::new();
    for p in &rule.paths {
        if !ctx.applies(p.os) {
            continue;
        }
        let patterns = p.patterns.clone().unwrap_or_else(|| rule.patterns.clone());
        for dir in ctx.existing_dirs(&p.path)? {
            if out.iter().any(|l| l.root == dir) {
                continue;
            }
            out.push(make_location(dir, patterns.clone(), LocationSource::Rule { rule: rule.id.clone() })?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rules_are_valid_and_cover_well_known_games() {
        let set = RuleSet::builtin().unwrap();
        assert!(set.rules().len() >= 21);
        for id in [
            413150, 105600, 367520, 1245620, 374320, 1145360, 504230, 892970, 526870, 1623730, 1091500, 292030, 489830,
            377160, 1086940, 294100, 427520, 548430, 264710, 646570, 739630,
        ] {
            assert!(set.matching(Some(id), "").next().is_some(), "no rule for {id}");
        }
        let sdv = set.get("stardew-valley").unwrap();
        assert_eq!(sdv.paths[0].path, "<winAppData>/StardewValley/Saves");
        assert_eq!(sdv.paths[1].os, PathOs::Linux);
    }

    #[test]
    fn invalid_rules_are_rejected() {
        let bad_placeholder = "[[game]]\nid = \"x\"\nname = \"X\"\npaths = [{ path = \"<nope>/a\" }]\n";
        assert!(matches!(RuleSet::from_toml(bad_placeholder, "t"), Err(DiscoveryError::Rules { .. })));
        let dup = "[[game]]\nid = \"x\"\nname = \"X\"\npaths = [{ path = \"<home>\" }]\n[[game]]\nid = \"x\"\nname = \"Y\"\npaths = [{ path = \"<home>\" }]\n";
        assert!(RuleSet::from_toml(dup, "t").is_err());
        let no_paths = "[[game]]\nid = \"x\"\nname = \"X\"\npaths = []\n";
        assert!(RuleSet::from_toml(no_paths, "t").is_err());
        let bad_os = "[[game]]\nid = \"x\"\nname = \"X\"\npaths = [{ path = \"<home>\", os = \"mac\" }]\n";
        assert!(RuleSet::from_toml(bad_os, "t").is_err());
        let bad_pattern = "[[game]]\nid = \"x\"\nname = \"X\"\npatterns = [\"[\"]\npaths = [{ path = \"<home>\" }]\n";
        assert!(RuleSet::from_toml(bad_pattern, "t").is_err());
    }

    #[test]
    fn merge_replaces_and_appends() {
        let mut set = RuleSet::builtin().unwrap();
        let n = set.rules().len();
        let user = RuleSet::from_toml(
            "[[game]]\nid = \"stardew-valley\"\nname = \"Stardew Valley\"\napp_ids = [413150]\npaths = [{ path = \"<home>/sdv\" }]\n[[game]]\nid = \"custom\"\nname = \"My Game\"\npaths = [{ path = \"<base>/s\" }]\n",
            "user",
        )
        .unwrap();
        set.merge(user);
        assert_eq!(set.rules().len(), n + 1);
        assert_eq!(set.get("stardew-valley").unwrap().paths.len(), 1);
        assert!(set.matching(None, "my game").next().is_some());
    }
}
