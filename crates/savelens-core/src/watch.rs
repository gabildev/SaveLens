//! Change watch: turns the semantic diff of a game save into a short list of
//! "what is this variable?" questions, and filters out everything the player
//! does not care about (play time, timestamps, coordinates, RNG seeds, ids,
//! world-state lists, counters that move on every single save, …).
//!
//! Pure logic: the watcher daemon feeds it diffs and stores the answers.
//!
//! Screening order for every changed value of a save:
//!
//! 1. only numeric changes are considered (ints, floats, numeric strings and
//!    small raw-byte changes seen as `i32` words); booleans and text are not;
//! 2. values already in the cheat table, muted patterns and pending questions
//!    are skipped;
//! 3. name heuristics reject time / position / id / seed / version / checksum
//!    fields, value heuristics reject Unix timestamps;
//! 4. bulk changes (many elements of the same list changing in one save, or a
//!    raw blob with many changed words) are world state, not player stats,
//!    whatever their names;
//! 5. volatile values (changed on every one of the last N saves, always in
//!    the same direction) are auto-muted unless their name looks like a stat;
//! 6. the survivors are ranked (name hints first) and only the best few are
//!    asked per save.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::diff::{ChangeKind, SemanticDiff};
use crate::node::{Node, ScalarValue};
use crate::path::{NodePath, PathSeg};
use crate::raw::{Endianness, ScanType};
use crate::scanner::Location;

/// What a variable means to the player. Covers the stats of every common
/// genre (RPG, shooter, survival, strategy, sim, platformer, …); anything
/// else is [`VariableCategory::Other`] with the player's own name as alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VariableCategory {
    // Resources
    Money,
    PremiumCurrency,
    Materials,
    Food,
    Fuel,
    // Vitals
    Health,
    MaxHealth,
    Armor,
    Mana,
    Stamina,
    Lives,
    Survival,
    // Progression
    Experience,
    Level,
    SkillPoints,
    Attribute,
    Reputation,
    QuestProgress,
    Unlock,
    // Inventory
    Ammo,
    Items,
    Keys,
    Durability,
    // Combat
    Damage,
    Speed,
    Score,
    CombatStats,
    // World
    Population,
    Time,
    Other,
}

/// Menu group of a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CategoryGroup {
    Resources,
    Vitals,
    Progression,
    Inventory,
    Combat,
    World,
}

impl CategoryGroup {
    pub const ALL: [CategoryGroup; 6] = [
        CategoryGroup::Resources,
        CategoryGroup::Vitals,
        CategoryGroup::Progression,
        CategoryGroup::Inventory,
        CategoryGroup::Combat,
        CategoryGroup::World,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CategoryGroup::Resources => "Resources",
            CategoryGroup::Vitals => "Vitals",
            CategoryGroup::Progression => "Progression",
            CategoryGroup::Inventory => "Inventory",
            CategoryGroup::Combat => "Combat & score",
            CategoryGroup::World => "World & other",
        }
    }

    /// Categories of the group, in menu order.
    pub fn categories(self) -> impl Iterator<Item = VariableCategory> {
        VariableCategory::ALL.into_iter().filter(move |c| c.group() == self)
    }
}

struct CategoryInfo {
    cat: VariableCategory,
    id: &'static str,
    label: &'static str,
    group: CategoryGroup,
    /// Name words that suggest the category (English plus common Spanish).
    words: &'static [&'static str],
}

use CategoryGroup as G;
use VariableCategory as C;

const CATEGORIES: &[CategoryInfo] = &[
    CategoryInfo {
        cat: C::Money,
        id: "money",
        label: "Money",
        group: G::Resources,
        words: &[
            "gold", "money", "cash", "coin", "coins", "credit", "credits", "funds", "currency", "dinero", "oro",
            "monedas", "dollars", "bucks", "souls", "runes", "zeny", "gil", "caps", "silver", "wealth", "bank",
            "balance", "rupees", "geo", "gp", "bells", "zenny", "munny", "lumens", "eddies", "shekels", "bits",
            "pesos", "euros", "yen", "marks", "crowns", "septims", "dosh", "loot", "wallet", "purse",
        ],
    },
    CategoryInfo {
        cat: C::PremiumCurrency,
        id: "premium-currency",
        label: "Gems / premium",
        group: G::Resources,
        words: &[
            "gems",
            "gem",
            "diamonds",
            "crystals",
            "crystal",
            "tokens",
            "token",
            "premium",
            "vbucks",
            "platinum",
            "rubies",
            "jewels",
            "shards",
            "orbs",
            "stars",
            "tickets",
            "gemas",
            "cristales",
            "fichas",
        ],
    },
    CategoryInfo {
        cat: C::Materials,
        id: "materials",
        label: "Materials",
        group: G::Resources,
        words: &[
            "wood",
            "stone",
            "ore",
            "iron",
            "copper",
            "steel",
            "metal",
            "scrap",
            "materials",
            "material",
            "resources",
            "resource",
            "crafting",
            "fiber",
            "cloth",
            "leather",
            "herbs",
            "sticks",
            "logs",
            "planks",
            "bricks",
            "clay",
            "sand",
            "coal",
            "sulfur",
            "salvage",
            "parts",
            "components",
            "madera",
            "piedra",
            "hierro",
            "materiales",
            "recursos",
            "mineral",
            "minerals",
            "essence",
            "dust",
        ],
    },
    CategoryInfo {
        cat: C::Food,
        id: "food",
        label: "Food / supplies",
        group: G::Resources,
        words: &["food", "meat", "rations", "supplies", "provisions", "bread", "grain", "wheat", "berries", "comida"],
    },
    CategoryInfo {
        cat: C::Fuel,
        id: "fuel",
        label: "Fuel / battery",
        group: G::Resources,
        words: &["fuel", "gas", "gasoline", "petrol", "oil", "battery", "charge", "electricity", "combustible"],
    },
    CategoryInfo {
        cat: C::Health,
        id: "health",
        label: "Health",
        group: G::Vitals,
        words: &["hp", "health", "life", "vida", "salud", "vitality", "hitpoints", "heart"],
    },
    CategoryInfo {
        cat: C::MaxHealth,
        id: "max-health",
        label: "Max health",
        group: G::Vitals,
        words: &["maxhp", "maxhealth", "healthmax", "hpmax", "maxlife"],
    },
    CategoryInfo {
        cat: C::Armor,
        id: "armor",
        label: "Armor / shield",
        group: G::Vitals,
        words: &[
            "armor",
            "armour",
            "shield",
            "shields",
            "defense",
            "defence",
            "def",
            "toughness",
            "barrier",
            "guard",
            "poise",
            "block",
            "armadura",
            "escudo",
            "defensa",
        ],
    },
    CategoryInfo {
        cat: C::Mana,
        id: "mana",
        label: "Mana / energy",
        group: G::Vitals,
        words: &[
            "mp", "mana", "energy", "fp", "focus", "rage", "magic", "magicka", "chakra", "ki", "spirit", "energia",
        ],
    },
    CategoryInfo {
        cat: C::Stamina,
        id: "stamina",
        label: "Stamina",
        group: G::Vitals,
        words: &["stamina", "endurance", "breath", "aguante", "resistencia", "sprint"],
    },
    CategoryInfo {
        cat: C::Lives,
        id: "lives",
        label: "Lives / continues",
        group: G::Vitals,
        words: &["lives", "continues", "revives", "tries", "attempts", "respawns", "vidas", "extralives", "oneups"],
    },
    CategoryInfo {
        cat: C::Survival,
        id: "survival",
        label: "Hunger / thirst / needs",
        group: G::Vitals,
        words: &[
            "hunger",
            "thirst",
            "oxygen",
            "sanity",
            "temperature",
            "temp",
            "fatigue",
            "sleep",
            "rest",
            "radiation",
            "rads",
            "poison",
            "infection",
            "nutrition",
            "hydration",
            "water",
            "warmth",
            "stress",
            "morale",
            "happiness",
            "bladder",
            "hygiene",
            "fun",
            "comfort",
            "hambre",
            "sed",
            "cordura",
            "oxigeno",
            "sueño",
        ],
    },
    CategoryInfo {
        cat: C::Experience,
        id: "experience",
        label: "XP",
        group: G::Progression,
        words: &["xp", "exp", "experience", "experiencia"],
    },
    CategoryInfo {
        cat: C::Level,
        id: "level",
        label: "Level",
        group: G::Progression,
        words: &["level", "lvl", "lv", "rank", "tier", "nivel", "prestige", "grade"],
    },
    CategoryInfo {
        cat: C::SkillPoints,
        id: "skill-points",
        label: "Skill / perk points",
        group: G::Progression,
        words: &[
            "skillpoints",
            "skill",
            "skills",
            "perk",
            "perks",
            "talent",
            "talents",
            "abilitypoints",
            "attributepoints",
            "upgradepoints",
            "research",
            "tech",
            "ap",
            "habilidad",
            "mastery",
        ],
    },
    CategoryInfo {
        cat: C::Attribute,
        id: "attribute",
        label: "Attribute (STR/DEX/…)",
        group: G::Progression,
        words: &[
            "strength",
            "str",
            "dexterity",
            "dex",
            "intelligence",
            "int",
            "agility",
            "agi",
            "luck",
            "luk",
            "wisdom",
            "charisma",
            "constitution",
            "perception",
            "vigor",
            "faith",
            "arcane",
            "attunement",
            "fuerza",
            "destreza",
            "inteligencia",
            "suerte",
            "sabiduria",
            "carisma",
        ],
    },
    CategoryInfo {
        cat: C::Reputation,
        id: "reputation",
        label: "Reputation / relationship",
        group: G::Progression,
        words: &[
            "reputation",
            "rep",
            "karma",
            "fame",
            "honor",
            "honour",
            "favor",
            "favour",
            "affinity",
            "relationship",
            "love",
            "friendship",
            "trust",
            "loyalty",
            "alignment",
            "infamy",
            "notoriety",
            "standing",
            "approval",
            "affection",
            "hearts",
            "reputacion",
            "amistad",
        ],
    },
    CategoryInfo {
        cat: C::QuestProgress,
        id: "quest-progress",
        label: "Quest / story progress",
        group: G::Progression,
        words: &[
            "quest",
            "quests",
            "mission",
            "missions",
            "stage",
            "progress",
            "chapter",
            "act",
            "objective",
            "story",
            "storyline",
            "episode",
            "phase",
            "mision",
            "capitulo",
            "progreso",
        ],
    },
    CategoryInfo {
        cat: C::Unlock,
        id: "unlock",
        label: "Unlock / flag",
        group: G::Progression,
        words: &[
            "unlocked",
            "unlock",
            "has",
            "owned",
            "own",
            "purchased",
            "bought",
            "learned",
            "learnt",
            "discovered",
            "found",
            "completed",
            "complete",
            "done",
            "finished",
            "defeated",
            "beaten",
            "opened",
            "collected",
            "obtained",
            "acquired",
            "achievement",
            "achieved",
            "visited",
            "met",
            "rescued",
            "activated",
            "solved",
            "cleared",
            "desbloqueado",
            "completado",
        ],
    },
    CategoryInfo {
        cat: C::Ammo,
        id: "ammo",
        label: "Ammo",
        group: G::Inventory,
        words: &[
            "ammo", "bullets", "arrows", "bolts", "shells", "rounds", "clip", "magazine", "mag", "grenades", "rockets",
            "missiles", "darts", "municion", "balas", "flechas",
        ],
    },
    CategoryInfo {
        cat: C::Items,
        id: "items",
        label: "Item count",
        group: G::Inventory,
        words: &[
            "count",
            "qty",
            "quantity",
            "stack",
            "stock",
            "amount",
            "num",
            "number",
            "potions",
            "potion",
            "bombs",
            "flasks",
            "estus",
            "elixirs",
            "medkits",
            "bandages",
            "stimpaks",
            "consumables",
            "items",
            "item",
            "inventory",
            "cantidad",
            "pociones",
            "objetos",
        ],
    },
    CategoryInfo {
        cat: C::Keys,
        id: "keys",
        label: "Keys / lockpicks",
        group: G::Inventory,
        words: &["keys", "key", "lockpicks", "lockpick", "picks", "keycards", "llaves"],
    },
    CategoryInfo {
        cat: C::Durability,
        id: "durability",
        label: "Durability",
        group: G::Inventory,
        words: &["durability", "condition", "wear", "integrity", "repair", "sharpness", "durabilidad"],
    },
    CategoryInfo {
        cat: C::Damage,
        id: "damage",
        label: "Damage / attack",
        group: G::Combat,
        words: &["damage", "dmg", "attack", "atk", "power", "crit", "critical", "might", "firepower", "dano", "ataque"],
    },
    CategoryInfo {
        cat: C::Speed,
        id: "speed",
        label: "Speed stat",
        group: G::Combat,
        words: &["movespeed", "walkspeed", "runspeed", "attackspeed", "haste", "quickness"],
    },
    CategoryInfo {
        cat: C::Score,
        id: "score",
        label: "Score / points",
        group: G::Combat,
        words: &["score", "points", "highscore", "puntos", "puntuacion", "combo", "multiplier", "medals", "trophies"],
    },
    CategoryInfo {
        cat: C::CombatStats,
        id: "combat-stats",
        label: "Kills / wins / deaths",
        group: G::Combat,
        words: &[
            "kills",
            "kill",
            "deaths",
            "death",
            "wins",
            "losses",
            "victories",
            "defeats",
            "headshots",
            "streak",
            "assists",
            "bounties",
            "bounty",
            "muertes",
            "victorias",
        ],
    },
    CategoryInfo {
        cat: C::Population,
        id: "population",
        label: "Population / units",
        group: G::World,
        words: &[
            "population",
            "pop",
            "citizens",
            "workers",
            "villagers",
            "colonists",
            "settlers",
            "units",
            "troops",
            "army",
            "soldiers",
            "followers",
            "crew",
            "pawns",
            "survivors",
            "residents",
            "poblacion",
            "tropas",
        ],
    },
    CategoryInfo {
        cat: C::Time,
        id: "time",
        label: "Day / time limit",
        group: G::World,
        words: &["daysleft", "deadline", "countdown", "turns", "turn", "moves", "dias"],
    },
    CategoryInfo { cat: C::Other, id: "other", label: "Other (name it)", group: G::World, words: &[] },
];

impl VariableCategory {
    /// Every category, in menu order.
    pub const ALL: [VariableCategory; 30] = [
        C::Money,
        C::PremiumCurrency,
        C::Materials,
        C::Food,
        C::Fuel,
        C::Health,
        C::MaxHealth,
        C::Armor,
        C::Mana,
        C::Stamina,
        C::Lives,
        C::Survival,
        C::Experience,
        C::Level,
        C::SkillPoints,
        C::Attribute,
        C::Reputation,
        C::QuestProgress,
        C::Unlock,
        C::Ammo,
        C::Items,
        C::Keys,
        C::Durability,
        C::Damage,
        C::Speed,
        C::Score,
        C::CombatStats,
        C::Population,
        C::Time,
        C::Other,
    ];

    fn info(self) -> &'static CategoryInfo {
        // The table lists every variant (checked by a unit test).
        CATEGORIES.iter().find(|i| i.cat == self).unwrap_or(&CATEGORIES[CATEGORIES.len() - 1])
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        self.info().label
    }

    /// Stable identifier (`money`, `max-health`, …).
    pub fn id(self) -> &'static str {
        self.info().id
    }

    /// Menu group.
    pub fn group(self) -> CategoryGroup {
        self.info().group
    }

    /// 1-based position in [`VariableCategory::ALL`].
    pub fn number(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).map(|i| i + 1).unwrap_or(Self::ALL.len())
    }

    /// Category from its position in [`VariableCategory::ALL`] (1-based).
    pub fn from_number(n: usize) -> Option<VariableCategory> {
        n.checked_sub(1).and_then(|i| Self::ALL.get(i).copied())
    }

    /// Parses a number, an id, a label or any name word of the category
    /// (`gold`, `vida`, `hunger`, `lockpicks`, …).
    pub fn parse(input: &str) -> Option<VariableCategory> {
        let s = input.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        if let Ok(n) = s.parse::<usize>() {
            return Self::from_number(n);
        }
        let compact: String = s.chars().filter(|c| c.is_alphanumeric()).collect();
        CATEGORIES
            .iter()
            .find(|i| {
                i.id == s
                    || i.label.to_ascii_lowercase() == s
                    || i.id.replace('-', "") == compact
                    || i.label.to_ascii_lowercase().split([' ', '/', '(']).next() == Some(s.as_str())
            })
            .or_else(|| {
                CATEGORIES.iter().find(|i| i.words.contains(&s.as_str()) || i.words.contains(&compact.as_str()))
            })
            .map(|i| i.cat)
    }

    /// Nine one-key choices for a question: the guessed category first, then
    /// the most common stats (flags get progression choices), `Other` last.
    pub fn quick_choices(guess: Option<VariableCategory>, flag: bool) -> Vec<VariableCategory> {
        let common: &[VariableCategory] = if flag {
            &[C::Unlock, C::QuestProgress, C::Items, C::Keys, C::Reputation, C::SkillPoints, C::Lives, C::Level]
        } else {
            &[C::Money, C::Health, C::Mana, C::Stamina, C::Ammo, C::Experience, C::Level, C::Items, C::Materials]
        };
        let mut out: Vec<VariableCategory> = guess.into_iter().collect();
        for c in common {
            if out.len() >= 8 {
                break;
            }
            if !out.contains(c) {
                out.push(*c);
            }
        }
        out.push(C::Other);
        out
    }
}

impl fmt::Display for VariableCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A category suggested from the variable name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryGuess {
    pub category: VariableCategory,
    /// 0–100.
    pub confidence: u8,
    /// Name word that triggered the guess.
    pub keyword: String,
}

/// Why a changed value was not turned into a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "detail", rename_all = "kebab-case")]
pub enum NoiseReason {
    /// Booleans, text, structure changes.
    NotNumeric,
    /// An on/off switch whose name does not say progress (weather, UI, settings).
    Switch,
    /// Already in the cheat table.
    Pinned,
    /// Matches a muted pattern.
    Muted(String),
    /// Already waiting for an answer (or snoozed).
    AlreadyAsked,
    /// Name says time / date / play time.
    TimeLike(String),
    /// Name says coordinates / rotation / velocity / camera.
    Spatial(String),
    /// Name says id / guid / hash / checksum / version / seed.
    Technical(String),
    /// Value looks like a Unix timestamp.
    TimestampValue,
    /// Many elements of the same list (or many raw words) changed at once.
    BulkChange { pattern: String, count: usize },
    /// Changed on every one of the last saves in the same direction.
    Volatile { streak: u32 },
    /// Cut by the per-save question limit.
    RateLimited,
}

impl fmt::Display for NoiseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoiseReason::NotNumeric => f.write_str("not a number"),
            NoiseReason::Switch => f.write_str("on/off switch without a progress name"),
            NoiseReason::Pinned => f.write_str("already in the cheat table"),
            NoiseReason::Muted(p) => write!(f, "muted by `{p}`"),
            NoiseReason::AlreadyAsked => f.write_str("already asked"),
            NoiseReason::TimeLike(w) => write!(f, "time-like name ({w})"),
            NoiseReason::Spatial(w) => write!(f, "position/rotation name ({w})"),
            NoiseReason::Technical(w) => write!(f, "technical field ({w})"),
            NoiseReason::TimestampValue => f.write_str("value looks like a timestamp"),
            NoiseReason::BulkChange { pattern, count } => write!(f, "{count} values of `{pattern}` changed at once"),
            NoiseReason::Volatile { streak } => write!(f, "changes on every save ({streak} in a row)"),
            NoiseReason::RateLimited => f.write_str("question limit for this save reached"),
        }
    }
}

// ----------------------------------------------------------------------
// Names
// ----------------------------------------------------------------------

/// Key segments whose own name says nothing (`timePlayed.value`).
const GENERIC_KEYS: &[&str] =
    &["value", "val", "v", "current", "cur", "data", "amount_value", "base", "raw", "inner", "_value", "m_value"];

/// Splits a key into lower-case words: `playerGoldCoins` → `player gold coins`,
/// `store.cash1` → `store cash`, `MAX_HP` → `max hp`.
pub fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = key.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() || c.is_ascii_digit() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let boundary = c.is_uppercase()
            && !cur.is_empty()
            && (chars[i - 1].is_lowercase() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
        if boundary {
            words.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// The meaningful name words of a path, most specific first: the words of
/// the last non-generic key segment, last word first (`Stats.CurrentHP` →
/// `hp current`, `timePlayed.value` → `played time`).
pub fn name_words(path: &NodePath) -> Vec<String> {
    let keys: Vec<&str> = path.segments().iter().filter_map(PathSeg::as_key).collect();
    let pick = keys
        .iter()
        .rev()
        .find(|k| !GENERIC_KEYS.contains(&k.to_ascii_lowercase().as_str()) && !key_words(k).is_empty())
        .or(keys.last());
    let mut words = pick.map(|k| key_words(k)).unwrap_or_default();
    words.reverse();
    words
}

/// Category hint from the variable name.
pub fn guess_category(path: &NodePath) -> Option<CategoryGuess> {
    let words = name_words(path);
    let joined: String = words.iter().rev().cloned().collect();
    let refine = |cat: VariableCategory| {
        // `MaxHP`, `HealthMax`: the maximum, not the current value.
        let max = words.iter().any(|w| matches!(w.as_str(), "max" | "maximum" | "cap" | "limit"));
        if cat == C::Health && max {
            C::MaxHealth
        } else {
            cat
        }
    };
    // Whole compound name first (`skillPoints`, `moveSpeed`, `maxHP`).
    if words.len() > 1 {
        if let Some(i) = CATEGORIES.iter().find(|i| i.words.contains(&joined.as_str())) {
            return Some(CategoryGuess { category: refine(i.cat), confidence: 90, keyword: joined });
        }
    }
    // The last (most specific) word wins; exact word match beats substring.
    for (pos, word) in words.iter().enumerate() {
        if let Some(i) = CATEGORIES.iter().find(|i| i.words.contains(&word.as_str())) {
            let confidence = if pos == 0 { 90 } else { 70 };
            return Some(CategoryGuess { category: refine(i.cat), confidence, keyword: word.clone() });
        }
    }
    for word in &words {
        for i in CATEGORIES {
            if let Some(k) = i.words.iter().find(|k| k.len() >= 4 && word.contains(*k)) {
                return Some(CategoryGuess { category: refine(i.cat), confidence: 50, keyword: (*k).to_string() });
            }
        }
    }
    None
}

/// Booleans are only asked about when their name says progress
/// (`hasDoubleJump`, `bossDefeated`, `mapUnlocked`); weather / UI / setting
/// switches are ignored.
pub fn flag_guess(path: &NodePath) -> Option<CategoryGuess> {
    let words = name_words(path);
    let info = CATEGORIES.iter().find(|i| i.cat == C::Unlock)?;
    words.iter().find(|w| info.words.contains(&w.as_str())).map(|w| CategoryGuess {
        category: C::Unlock,
        confidence: 80,
        keyword: w.clone(),
    })
}

const TIME_WORDS: &[&str] = &[
    "time",
    "timer",
    "timestamp",
    "date",
    "day",
    "days",
    "hour",
    "hours",
    "minute",
    "minutes",
    "second",
    "seconds",
    "sec",
    "secs",
    "ms",
    "tick",
    "ticks",
    "elapsed",
    "played",
    "playtime",
    "duration",
    "clock",
    "utc",
    "epoch",
    "frame",
    "frames",
    "ut",
    "now",
    "last",
    "updated",
    "saved",
    "dt",
    "delta",
    "age",
    "tiempo",
    "fecha",
];
const SPATIAL_WORDS: &[&str] = &[
    "x",
    "y",
    "z",
    "w",
    "pos",
    "position",
    "location",
    "loc",
    "coord",
    "coords",
    "rotation",
    "rot",
    "yaw",
    "pitch",
    "roll",
    "angle",
    "heading",
    "velocity",
    "vel",
    "speed",
    "camera",
    "cam",
    "zoom",
    "scale",
    "offset",
    "lat",
    "lon",
    "altitude",
    "orbit",
    "translation",
    "quat",
    "transform",
    "dir",
    "direction",
];
const TECHNICAL_WORDS: &[&str] = &[
    "id", "uid", "guid", "uuid", "hash", "checksum", "crc", "version", "ver", "build", "seed", "rng", "random", "rand",
    "index", "idx", "handle", "ptr", "flags", "mask", "revision", "serial", "nonce", "salt", "steam", "save", "slot",
];

/// Words that make a compound name bookkeeping whatever else it says:
/// `saveCount`, `timesLoaded`, `goldSeed`, `playtimeHours`, `levelVersion`.
const HARD_NOISE_WORDS: &[&str] = &[
    "save",
    "saved",
    "saves",
    "autosave",
    "load",
    "loaded",
    "loads",
    "played",
    "playtime",
    "timestamp",
    "seed",
    "version",
    "checksum",
    "crc",
    "guid",
    "uuid",
    "hash",
    "boot",
    "launch",
    "launches",
    "session",
    "sessions",
];

/// Bookkeeping names that are noise even when they also contain a stat
/// word (`saveCount` has "count", `levelSeed` has "level").
pub fn hard_noise(path: &NodePath) -> Option<NoiseReason> {
    let words = name_words(path);
    let w = words.iter().find(|w| HARD_NOISE_WORDS.contains(&w.as_str()))?;
    let name = words.iter().rev().cloned().collect::<Vec<_>>().join(" ");
    Some(if matches!(w.as_str(), "played" | "playtime" | "timestamp" | "session" | "sessions") {
        NoiseReason::TimeLike(name)
    } else {
        NoiseReason::Technical(name)
    })
}

/// Rejects time / spatial / technical fields by name.
pub fn name_noise(path: &NodePath) -> Option<NoiseReason> {
    let words = name_words(path);
    let leaf = words.first()?;
    if TIME_WORDS.contains(&leaf.as_str()) || words.iter().any(|w| matches!(w.as_str(), "timestamp" | "playtime")) {
        return Some(NoiseReason::TimeLike(leaf.clone()));
    }
    if SPATIAL_WORDS.contains(&leaf.as_str()) {
        return Some(NoiseReason::Spatial(leaf.clone()));
    }
    if TECHNICAL_WORDS.contains(&leaf.as_str()) {
        return Some(NoiseReason::Technical(leaf.clone()));
    }
    // Compound names ending in a time / technical word: `lastSaveTime`,
    // `worldSeed`, `itemGuid`, `playTimeSeconds`.
    if words.len() > 1 && words.iter().any(|w| matches!(w.as_str(), "time" | "seconds" | "date" | "played")) {
        return Some(NoiseReason::TimeLike(words.join(" ")));
    }
    None
}

/// Rejects values that look like Unix timestamps (seconds or milliseconds,
/// 2001–2096) or .NET ticks.
pub fn value_noise(old: ScalarValue, new: ScalarValue) -> Option<NoiseReason> {
    let looks = |v: ScalarValue| {
        let x = v.as_f64().abs();
        (1.0e9..4.0e9).contains(&x) || (1.0e12..4.0e12).contains(&x) || (6.0e17..7.0e17).contains(&x)
    };
    (looks(old) && looks(new)).then_some(NoiseReason::TimestampValue)
}

// ----------------------------------------------------------------------
// Mute patterns
// ----------------------------------------------------------------------

/// A muted path pattern. `*` matches any run of characters (including `.`),
/// so `world.chunks[*].*` silences a whole list and `*.timePlayed` a name
/// anywhere. Raw locations are matched as `<region>@0x<offset>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MutePattern(pub String);

impl MutePattern {
    /// Pattern matching exactly one location.
    pub fn exact(location: &Location) -> MutePattern {
        MutePattern(location_key(location))
    }

    /// Pattern matching every element of the lists the location is in
    /// (`enemies[3].hp` → `enemies[*].hp`).
    pub fn generalized(location: &Location) -> MutePattern {
        match location {
            Location::Ast { path } => MutePattern(generalize_path(path)),
            Location::Raw { .. } => MutePattern::exact(location),
        }
    }

    /// True when the pattern matches `location`.
    pub fn matches(&self, location: &Location) -> bool {
        glob_match(self.0.as_bytes(), location_key(location).as_bytes())
    }
}

impl fmt::Display for MutePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable text key of a location (used for mute patterns and storage).
pub fn location_key(location: &Location) -> String {
    match location {
        Location::Ast { path } => path.to_string(),
        Location::Raw { region, offset, ty, endian } => {
            let region = if region.is_root() { String::new() } else { region.to_string() };
            format!("{region}@{offset:#x}:{ty:?}:{endian:?}")
        }
    }
}

/// `a[3].b[0].c` → `a[*].b[*].c`.
pub fn generalize_path(path: &NodePath) -> String {
    let mut s = String::new();
    for seg in path.segments() {
        match seg {
            PathSeg::Index(_) => s.push_str("[*]"),
            PathSeg::Key(_) => {
                let one = NodePath::from_segments(vec![seg.clone()]).to_string();
                if !s.is_empty() && !one.starts_with('[') {
                    s.push('.');
                }
                s.push_str(&one);
            }
        }
    }
    s
}

fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    // Iterative wildcard matching with backtracking on the last `*`.
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pat.len() && pat[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pat.len() && pat[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|c| *c == b'*')
}

// ----------------------------------------------------------------------
// Volatility
// ----------------------------------------------------------------------

/// Per-variable change history across saves of one file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volatility {
    /// Save counter of the file when the value last changed.
    pub last_save: u64,
    /// Consecutive saves (ending at `last_save`) in which the value changed.
    pub streak: u32,
    /// Consecutive changes that went in the same direction.
    pub same_direction: u32,
    /// Direction of the last change (+1 / -1).
    pub direction: i8,
    /// Total number of saves in which the value changed.
    pub changes: u32,
}

impl Volatility {
    /// Records a change in save number `save` with the sign of the delta.
    pub fn observe(&mut self, save: u64, delta: f64) {
        let dir: i8 = if delta >= 0.0 { 1 } else { -1 };
        let consecutive = self.changes > 0 && save == self.last_save + 1;
        self.streak = if consecutive { self.streak + 1 } else { 1 };
        self.same_direction = if consecutive && dir == self.direction { self.same_direction + 1 } else { 1 };
        self.direction = dir;
        self.last_save = save;
        self.changes += 1;
    }

    /// Clocks, step counters and "times saved": changed on every one of the
    /// last `threshold` saves, always in the same direction. Values whose
    /// name looks like a stat are only considered volatile with twice the
    /// streak (money can go up on every save for a while).
    pub fn is_volatile(&self, threshold: u32, has_hint: bool) -> bool {
        let need = if has_hint { threshold.saturating_mul(2) } else { threshold };
        threshold > 0 && self.streak >= need && self.same_direction >= need
    }
}

// ----------------------------------------------------------------------
// Screening
// ----------------------------------------------------------------------

/// Screening options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchOptions {
    /// At most this many questions per save of one file.
    pub max_questions_per_save: usize,
    /// More than this many changed elements of one list in one save is
    /// world state (six enemies losing hp), whatever their name.
    pub bulk_threshold: usize,
    /// Raw blobs with more changed words than this are ignored.
    pub max_raw_words: usize,
    /// Saves in a row (same direction) after which a value is volatile.
    pub volatile_streak: u32,
    /// Ask about raw byte changes (files without structure).
    pub raw_changes: bool,
    /// Only ask about values whose name looks like a stat.
    pub only_named_stats: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions {
            max_questions_per_save: 3,
            bulk_threshold: 4,
            max_raw_words: 6,
            volatile_streak: 4,
            raw_changes: true,
            only_named_stats: false,
        }
    }
}

/// A changed value worth asking about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchCandidate {
    pub location: Location,
    pub old: ScalarValue,
    pub new: ScalarValue,
    pub guess: Option<CategoryGuess>,
    pub score: i32,
    /// An on/off switch (boolean) seen as 0 / 1.
    #[serde(default)]
    pub flag: bool,
}

impl WatchCandidate {
    /// One-key answer choices for this question.
    pub fn quick_choices(&self) -> Vec<VariableCategory> {
        VariableCategory::quick_choices(self.guess.as_ref().map(|g| g.category), self.flag)
    }

    /// Short label (`player.money` or `@0x1a4 (i32)`).
    pub fn label(&self) -> String {
        match &self.location {
            Location::Ast { path } => path.to_string(),
            Location::Raw { region, offset, ty, .. } => {
                if region.is_root() {
                    format!("@{offset:#x} ({ty:?})")
                } else {
                    format!("{region} @{offset:#x} ({ty:?})")
                }
            }
        }
    }

    /// `1500 -> 1300 (-200)`.
    pub fn change_text(&self) -> String {
        if self.flag {
            let word = |v: ScalarValue| if v.as_i128() == Some(0) { "OFF" } else { "ON" };
            return format!("{} -> {}", word(self.old), word(self.new));
        }
        format!(
            "{} -> {} ({})",
            scalar_text(self.old),
            scalar_text(self.new),
            crate::diff::format_delta(self.new.delta(self.old))
        )
    }
}

fn scalar_text(v: ScalarValue) -> String {
    match v {
        ScalarValue::Int(i) => i.to_string(),
        ScalarValue::Float(f) => crate::node::format_float(f),
    }
}

/// What the screening needs to know about the game's current state.
pub trait WatchState {
    /// Location is in the cheat table.
    fn is_pinned(&self, location: &Location) -> bool;
    /// First muted pattern matching the location.
    fn muted_by(&self, location: &Location) -> Option<String>;
    /// A question about the location is pending or snoozed.
    fn already_asked(&self, location: &Location) -> bool;
    /// Change history of the location, including the current save.
    fn volatility(&self, location: &Location) -> Volatility;
}

/// Result of screening one save.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Screening {
    /// Questions to ask, best first.
    pub questions: Vec<WatchCandidate>,
    /// Every numeric change seen (for volatility bookkeeping).
    pub observed: Vec<NumericChange>,
    /// Suppressed changes and why.
    pub suppressed: Vec<(String, NoiseReason)>,
}

fn numeric(node: &Node) -> Option<ScalarValue> {
    match node {
        Node::Bool(_) => None,
        other => other.as_scalar_lenient().filter(|v| !v.is_nan()),
    }
}

/// A numeric change (booleans count as 0 / 1 with `flag` set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NumericChange {
    pub location: Location,
    pub old: ScalarValue,
    pub new: ScalarValue,
    pub flag: bool,
}

/// Extracts every numeric change of a diff as `(location, old, new)`.
/// Raw blobs contribute their changed `i32` words when there are few of them;
/// blobs with more are reported as bulk.
pub fn numeric_changes(diff: &SemanticDiff, opts: &WatchOptions) -> (Vec<NumericChange>, Vec<(String, NoiseReason)>) {
    let mut out = Vec::new();
    let mut suppressed = Vec::new();
    for change in &diff.changes {
        match &change.kind {
            ChangeKind::Modified { old: Node::Bool(o), new: Node::Bool(n) } => {
                if flag_guess(&change.path).is_some() || guess_category(&change.path).is_some() {
                    out.push(NumericChange {
                        location: Location::Ast { path: change.path.clone() },
                        old: ScalarValue::Int(*o as i128),
                        new: ScalarValue::Int(*n as i128),
                        flag: true,
                    });
                } else {
                    suppressed.push((change.path.to_string(), NoiseReason::Switch));
                }
            }
            ChangeKind::Modified { old, new } => match (numeric(old), numeric(new)) {
                (Some(o), Some(n)) => out.push(NumericChange {
                    location: Location::Ast { path: change.path.clone() },
                    old: o,
                    new: n,
                    flag: false,
                }),
                _ => suppressed.push((change.path.to_string(), NoiseReason::NotNumeric)),
            },
            ChangeKind::BytesChanged { ranges, truncated, old_len, new_len } => {
                if !opts.raw_changes {
                    continue;
                }
                let words: Vec<(u64, i32, i32)> = ranges.iter().filter_map(|r| r.word_view).collect();
                if *truncated || old_len != new_len || words.len() != ranges.len() || words.len() > opts.max_raw_words {
                    suppressed.push((
                        change.path.to_string(),
                        NoiseReason::BulkChange { pattern: format!("{} bytes", change.path), count: ranges.len() },
                    ));
                    continue;
                }
                for (offset, o, n) in words {
                    let location = Location::Raw {
                        region: change.path.clone(),
                        offset,
                        ty: ScanType::I32,
                        endian: Endianness::Little,
                    };
                    out.push(NumericChange {
                        location,
                        old: ScalarValue::Int(o as i128),
                        new: ScalarValue::Int(n as i128),
                        flag: false,
                    });
                }
            }
            ChangeKind::Added { .. } | ChangeKind::Removed { .. } | ChangeKind::TypeChanged { .. } => {
                suppressed.push((change.path.to_string(), NoiseReason::NotNumeric))
            }
        }
    }
    (out, suppressed)
}

fn score(location: &Location, old: ScalarValue, new: ScalarValue, guess: Option<&CategoryGuess>) -> i32 {
    let mut s = 10;
    if let Some(g) = guess {
        s += g.confidence as i32;
    }
    let integral = |v: ScalarValue| v.as_i128().is_some();
    if integral(old) && integral(new) {
        s += 10;
    } else {
        s -= 10;
    }
    let mag = new.as_f64().abs().max(old.as_f64().abs());
    if mag > 1.0e9 {
        s -= 20;
    }
    if new.as_f64() < 0.0 {
        s -= 5;
    }
    match location {
        Location::Raw { .. } => s -= 15,
        Location::Ast { path } => {
            if path.segments().iter().any(|seg| matches!(seg, PathSeg::Index(_))) {
                s -= 5;
            }
        }
    }
    s
}

/// Screens the numeric changes of one save.
pub fn screen(diff: &SemanticDiff, state: &dyn WatchState, opts: &WatchOptions) -> Screening {
    let (observed, mut suppressed) = numeric_changes(diff, opts);

    // Bulk detection: group AST changes by generalized pattern.
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for ch in &observed {
        if let Location::Ast { path } = &ch.location {
            if path.segments().iter().any(|s| matches!(s, PathSeg::Index(_))) {
                *groups.entry(generalize_path(path)).or_default() += 1;
            }
        }
    }

    let mut candidates = Vec::new();
    for ch in &observed {
        let (loc, old, new) = (&ch.location, &ch.old, &ch.new);
        let label = location_key(loc);
        let (path, guess) = match loc {
            Location::Ast { path } if ch.flag => (Some(path), flag_guess(path).or_else(|| guess_category(path))),
            Location::Ast { path } => (Some(path), guess_category(path)),
            Location::Raw { .. } => (None, None),
        };
        let reject = |r: NoiseReason, suppressed: &mut Vec<(String, NoiseReason)>| suppressed.push((label.clone(), r));
        if state.is_pinned(loc) {
            reject(NoiseReason::Pinned, &mut suppressed);
            continue;
        }
        if let Some(p) = state.muted_by(loc) {
            reject(NoiseReason::Muted(p), &mut suppressed);
            continue;
        }
        if state.already_asked(loc) {
            reject(NoiseReason::AlreadyAsked, &mut suppressed);
            continue;
        }
        if let Some(path) = path {
            if let Some(r) = hard_noise(path) {
                reject(r, &mut suppressed);
                continue;
            }
            // A clear stat name beats a noisy compound (`goldTotal` is money,
            // `playTimeSeconds` is time).
            if guess.as_ref().is_none_or(|g| g.confidence < 90) {
                if let Some(r) = name_noise(path) {
                    reject(r, &mut suppressed);
                    continue;
                }
            }
        }
        if let Some(r) = value_noise(*old, *new) {
            reject(r, &mut suppressed);
            continue;
        }
        if let Some(path) = path {
            let pattern = generalize_path(path);
            if let Some(&count) = groups.get(&pattern) {
                if count > opts.bulk_threshold {
                    reject(NoiseReason::BulkChange { pattern, count }, &mut suppressed);
                    continue;
                }
            }
        }
        let vol = state.volatility(loc);
        if vol.is_volatile(opts.volatile_streak, guess.as_ref().is_some_and(|g| g.confidence >= 70)) {
            reject(NoiseReason::Volatile { streak: vol.streak }, &mut suppressed);
            continue;
        }
        if opts.only_named_stats && guess.is_none() {
            reject(NoiseReason::NotNumeric, &mut suppressed);
            continue;
        }
        let score = score(loc, *old, *new, guess.as_ref());
        candidates.push(WatchCandidate { location: loc.clone(), old: *old, new: *new, guess, score, flag: ch.flag });
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label().cmp(&b.label())));
    for extra in candidates.split_off(opts.max_questions_per_save.min(candidates.len())) {
        suppressed.push((location_key(&extra.location), NoiseReason::RateLimited));
    }
    Screening { questions: candidates, observed, suppressed }
}

/// Default alias for an answered question: `Money` for `player.gold`
/// answered as money, `Health (hp)` when the name adds information.
pub fn default_alias(category: VariableCategory, location: &Location) -> String {
    let leaf = match location {
        Location::Ast { path } => {
            let words = name_words(path);
            if words.is_empty() {
                path.leaf_label()
            } else {
                words.iter().rev().cloned().collect::<Vec<_>>().join(" ")
            }
        }
        Location::Raw { offset, .. } => format!("@{offset:#x}"),
    };
    if category == VariableCategory::Other || leaf.is_empty() {
        return if leaf.is_empty() { category.label().to_string() } else { leaf };
    }
    if leaf.eq_ignore_ascii_case(category.label()) {
        category.label().to_string()
    } else {
        format!("{} ({leaf})", category.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::diff;
    use std::collections::HashMap;

    fn p(s: &str) -> NodePath {
        NodePath::parse(s).unwrap()
    }

    #[derive(Default)]
    struct State {
        pinned: Vec<Location>,
        muted: Vec<MutePattern>,
        asked: Vec<Location>,
        vol: HashMap<String, Volatility>,
    }

    impl WatchState for State {
        fn is_pinned(&self, l: &Location) -> bool {
            self.pinned.contains(l)
        }
        fn muted_by(&self, l: &Location) -> Option<String> {
            self.muted.iter().find(|m| m.matches(l)).map(|m| m.0.clone())
        }
        fn already_asked(&self, l: &Location) -> bool {
            self.asked.contains(l)
        }
        fn volatility(&self, l: &Location) -> Volatility {
            self.vol.get(&location_key(l)).copied().unwrap_or_default()
        }
    }

    fn map(entries: Vec<(&str, Node)>) -> Node {
        let mut m = crate::node::NodeMap::new();
        for (k, v) in entries {
            m.insert(k.to_string(), v);
        }
        Node::Map(m)
    }

    #[test]
    fn words_and_categories() {
        assert_eq!(key_words("playerGoldCoins"), ["player", "gold", "coins"]);
        assert_eq!(key_words("MAX_HP"), ["max", "hp"]);
        assert_eq!(key_words("store.cash1"), ["store", "cash"]);
        assert_eq!(key_words("XPTotal"), ["xp", "total"]);
        assert_eq!(name_words(&p("timePlayed.value")), ["played", "time"]);
        assert_eq!(name_words(&p("Stats.CurrentHP")), ["hp", "current"]);
        assert_eq!(guess_category(&p("player.money")).unwrap().category, VariableCategory::Money);
        assert_eq!(guess_category(&p(r#"log[0]["store.cash"]"#)).unwrap().category, VariableCategory::Money);
        assert_eq!(guess_category(&p("Stats.CurrentHP")).unwrap().category, VariableCategory::Health);
        assert_eq!(guess_category(&p("SaveData.goldpieces")).unwrap().confidence, 50);
        assert!(guess_category(&p("world.chunk")).is_none());
        assert_eq!(VariableCategory::parse("dinero"), Some(VariableCategory::Money));
        assert_eq!(VariableCategory::parse("1"), Some(VariableCategory::Money));
        assert_eq!(VariableCategory::parse("vida"), Some(VariableCategory::Health));
        assert_eq!(VariableCategory::parse("hunger"), Some(VariableCategory::Survival));
        assert_eq!(VariableCategory::parse("max-health"), Some(VariableCategory::MaxHealth));
        assert_eq!(VariableCategory::parse("skillpoints"), Some(VariableCategory::SkillPoints));
        assert_eq!(VariableCategory::parse("Ammo"), Some(VariableCategory::Ammo));
        assert_eq!(VariableCategory::from_number(VariableCategory::Score.number()), Some(VariableCategory::Score));
        assert_eq!(VariableCategory::parse("nope"), None);
        assert_eq!(VariableCategory::parse(""), None);
    }

    #[test]
    fn catalog_covers_every_category_once() {
        for c in VariableCategory::ALL {
            assert_eq!(CATEGORIES.iter().filter(|i| i.cat == c).count(), 1, "{c:?}");
            assert_eq!(VariableCategory::parse(c.id()), Some(c), "{c:?}");
            assert_eq!(serde_json::to_string(&c).unwrap().trim_matches('"'), c.id());
        }
        assert_eq!(CATEGORIES.len(), VariableCategory::ALL.len());
        let grouped: usize = CategoryGroup::ALL.iter().map(|g| g.categories().count()).sum();
        assert_eq!(grouped, VariableCategory::ALL.len());
        let q = VariableCategory::quick_choices(Some(VariableCategory::Survival), false);
        assert_eq!((q.len(), q[0], q[8]), (9, VariableCategory::Survival, VariableCategory::Other));
        let f = VariableCategory::quick_choices(None, true);
        assert_eq!((f[0], f.len()), (VariableCategory::Unlock, 9));
    }

    #[test]
    fn guesses_across_genres() {
        let g = |s: &str| guess_category(&p(s)).map(|g| g.category);
        // RPG
        assert_eq!(g("Player.MaxHP"), Some(VariableCategory::MaxHealth));
        assert_eq!(g("Player.HealthMax"), Some(VariableCategory::MaxHealth));
        assert_eq!(g("Player.Mana"), Some(VariableCategory::Mana));
        assert_eq!(g("stats.Strength"), Some(VariableCategory::Attribute));
        assert_eq!(g("player.skillPoints"), Some(VariableCategory::SkillPoints));
        assert_eq!(g("Char.Souls"), Some(VariableCategory::Money));
        assert_eq!(g("inventory.estus"), Some(VariableCategory::Items));
        // Shooter
        assert_eq!(g("weapons[0].ammo"), Some(VariableCategory::Ammo));
        assert_eq!(g("stats.headshots"), Some(VariableCategory::CombatStats));
        assert_eq!(g("player.armor"), Some(VariableCategory::Armor));
        // Survival / crafting
        assert_eq!(g("vitals.hunger"), Some(VariableCategory::Survival));
        assert_eq!(g("vitals.Thirst"), Some(VariableCategory::Survival));
        assert_eq!(g("storage.wood"), Some(VariableCategory::Materials));
        assert_eq!(g("car.fuel"), Some(VariableCategory::Fuel));
        assert_eq!(g("tools[2].durability"), Some(VariableCategory::Durability));
        // Strategy / sim / social
        assert_eq!(g("colony.population"), Some(VariableCategory::Population));
        assert_eq!(g("npc.Abigail.friendship"), Some(VariableCategory::Reputation));
        assert_eq!(g("quests.mainQuest.stage"), Some(VariableCategory::QuestProgress));
        assert_eq!(g("shop.gems"), Some(VariableCategory::PremiumCurrency));
        assert_eq!(g("player.lockpicks"), Some(VariableCategory::Keys));
        assert_eq!(g("player.lives"), Some(VariableCategory::Lives));
        assert_eq!(g("arcade.highScore"), Some(VariableCategory::Score));
        assert_eq!(g("run.turnsLeft").or(g("run.turns")), Some(VariableCategory::Time));
        assert_eq!(g("hero.moveSpeed"), Some(VariableCategory::Speed));
        assert_eq!(g("hero.attack"), Some(VariableCategory::Damage));
    }

    #[test]
    fn progress_flags_are_asked_and_switches_are_not() {
        let old = map(vec![
            ("hasDoubleJump", Node::Bool(false)),
            ("bossDefeated", Node::Bool(false)),
            ("isRaining", Node::Bool(false)),
            ("showHud", Node::Bool(true)),
        ]);
        let new = map(vec![
            ("hasDoubleJump", Node::Bool(true)),
            ("bossDefeated", Node::Bool(true)),
            ("isRaining", Node::Bool(true)),
            ("showHud", Node::Bool(false)),
        ]);
        let s = screen(&diff(&old, &new), &State::default(), &WatchOptions::default());
        let asked: Vec<String> = s.questions.iter().map(|c| c.label()).collect();
        assert_eq!(asked, ["bossDefeated", "hasDoubleJump"], "{:?}", s.suppressed);
        let q = &s.questions[0];
        assert!(q.flag);
        assert_eq!(q.change_text(), "OFF -> ON");
        assert_eq!(q.guess.as_ref().unwrap().category, VariableCategory::Unlock);
        assert_eq!(q.quick_choices()[0], VariableCategory::Unlock);
        let why = |k: &str| s.suppressed.iter().find(|(l, _)| l == k).map(|(_, r)| r.clone());
        assert_eq!(why("isRaining"), Some(NoiseReason::Switch));
        assert_eq!(why("showHud"), Some(NoiseReason::Switch));
    }

    #[test]
    fn noise_by_name_and_value() {
        assert!(matches!(name_noise(&p("timePlayed.value")), Some(NoiseReason::TimeLike(_))));
        assert!(matches!(name_noise(&p("player.position.x")), Some(NoiseReason::Spatial(_))));
        assert!(matches!(name_noise(&p("worldSeed")), Some(NoiseReason::Technical(_))));
        assert!(matches!(name_noise(&p("GAME.FLIGHTSTATE.UT")), Some(NoiseReason::TimeLike(_))));
        assert!(matches!(name_noise(&p("lastSaveTime")), Some(NoiseReason::TimeLike(_))));
        assert!(name_noise(&p("player.gold")).is_none());
        assert!(hard_noise(&p("meta.saveCount")).is_some());
        assert!(hard_noise(&p("world.levelSeed")).is_some());
        assert!(hard_noise(&p("player.gold")).is_none());
        assert!(hard_noise(&p("player.hp")).is_none());
        assert!(value_noise(ScalarValue::Int(1_700_000_000), ScalarValue::Int(1_700_000_600)).is_some());
        assert!(value_noise(ScalarValue::Int(1500), ScalarValue::Int(1300)).is_none());
    }

    #[test]
    fn mute_patterns_and_generalization() {
        let loc = Location::Ast { path: p("enemies[3].hp") };
        assert_eq!(MutePattern::generalized(&loc).0, "enemies[*].hp");
        assert!(MutePattern::generalized(&loc).matches(&Location::Ast { path: p("enemies[17].hp") }));
        assert!(!MutePattern::exact(&loc).matches(&Location::Ast { path: p("enemies[17].hp") }));
        assert!(MutePattern("*.timePlayed*".into()).matches(&Location::Ast { path: p("a.b.timePlayed.value") }));
        assert!(MutePattern("world.*".into()).matches(&Location::Ast { path: p("world.chunks[2].x") }));
        assert!(!MutePattern("world.*".into()).matches(&Location::Ast { path: p("player.gold") }));
        let raw =
            Location::Raw { region: NodePath::root(), offset: 0x40, ty: ScanType::I32, endian: Endianness::Little };
        assert!(MutePattern::exact(&raw).matches(&raw));
        assert!(MutePattern("@0x4*".into()).matches(&raw));
        assert_eq!(generalize_path(&p(r#"log[0]["store.cash"]"#)), r#"log[*]["store.cash"]"#);
    }

    #[test]
    fn volatility_needs_a_monotonic_streak() {
        let mut v = Volatility::default();
        for save in 1..=4 {
            v.observe(save, 1.0);
        }
        assert!(v.is_volatile(4, false));
        assert!(!v.is_volatile(4, true), "stats need twice the streak");
        v.observe(5, -3.0);
        assert!(!v.is_volatile(4, false), "direction change resets");
        let mut gap = Volatility::default();
        gap.observe(1, 1.0);
        gap.observe(3, 1.0);
        assert_eq!(gap.streak, 1);
        assert_eq!(gap.changes, 2);
    }

    #[test]
    fn screening_keeps_stats_and_drops_noise() {
        let enemies_old = Node::Array((0..6).map(|i| map(vec![("hp", Node::I32(100 + i))])).collect());
        let enemies_new = Node::Array((0..6).map(|i| map(vec![("hp", Node::I32(50 + i))])).collect());
        let old = map(vec![
            ("gold", Node::I64(1500)),
            ("lvlProgress", Node::F64(0.25)),
            ("unknownA", Node::I32(7)),
            ("timePlayed", Node::F64(1000.5)),
            ("lastSave", Node::I64(1_700_000_000)),
            ("pos", map(vec![("x", Node::F32(1.0))])),
            ("isNight", Node::Bool(false)),
            ("pinnedHp", Node::I32(10)),
            ("enemies", enemies_old),
        ]);
        let new = map(vec![
            ("gold", Node::I64(1300)),
            ("lvlProgress", Node::F64(0.5)),
            ("unknownA", Node::I32(9)),
            ("timePlayed", Node::F64(1200.5)),
            ("lastSave", Node::I64(1_700_000_900)),
            ("pos", map(vec![("x", Node::F32(2.0))])),
            ("isNight", Node::Bool(true)),
            ("pinnedHp", Node::I32(5)),
            ("enemies", enemies_new),
        ]);
        let d = diff(&old, &new);
        let state = State { pinned: vec![Location::Ast { path: p("pinnedHp") }], ..State::default() };
        let s = screen(&d, &state, &WatchOptions::default());
        let asked: Vec<String> = s.questions.iter().map(|c| c.label()).collect();
        assert_eq!(asked[0], "gold", "{asked:?} {:?}", s.suppressed);
        assert_eq!(s.questions[0].guess.as_ref().unwrap().category, VariableCategory::Money);
        assert_eq!(s.questions[0].change_text(), "1500 -> 1300 (-200)");
        assert!(asked.contains(&"unknownA".to_string()));
        assert!(asked.len() <= 3);
        let why = |k: &str| s.suppressed.iter().find(|(l, _)| l == k).map(|(_, r)| r.clone());
        assert!(matches!(why("timePlayed"), Some(NoiseReason::TimeLike(_))));
        assert!(matches!(why("lastSave"), Some(NoiseReason::Technical(_))));
        assert!(matches!(why("pos.x"), Some(NoiseReason::Spatial(_))));
        assert_eq!(why("isNight"), Some(NoiseReason::Switch));
        assert_eq!(why("pinnedHp"), Some(NoiseReason::Pinned));
        // Six enemies' hp changed at once: world state, even though `hp`
        // is a stat name.
        let bulk = s.suppressed.iter().filter(|(_, r)| matches!(r, NoiseReason::BulkChange { .. })).count();
        assert_eq!(bulk, 6);
        assert_eq!(asked, ["gold", "lvlProgress", "unknownA"]);
        assert_eq!(s.observed.len(), 8 + 6 - 1);
    }

    #[test]
    fn muted_asked_volatile_and_rate_limit() {
        let old = map((0..6).map(|i| (["a", "b", "c", "d", "e", "f"][i], Node::I32(i as i32))).collect());
        let new = map((0..6).map(|i| (["a", "b", "c", "d", "e", "f"][i], Node::I32(i as i32 + 10))).collect());
        let d = diff(&old, &new);
        let mut vol = HashMap::new();
        vol.insert(
            "c".to_string(),
            Volatility { last_save: 9, streak: 5, same_direction: 5, direction: 1, changes: 5 },
        );
        let state = State {
            muted: vec![MutePattern("a".into())],
            asked: vec![Location::Ast { path: p("b") }],
            vol,
            ..State::default()
        };
        let opts = WatchOptions { max_questions_per_save: 2, ..WatchOptions::default() };
        let s = screen(&d, &state, &opts);
        assert_eq!(s.questions.len(), 2);
        let why = |k: &str| s.suppressed.iter().find(|(l, _)| l == k).map(|(_, r)| r.clone());
        assert_eq!(why("a"), Some(NoiseReason::Muted("a".into())));
        assert_eq!(why("b"), Some(NoiseReason::AlreadyAsked));
        assert_eq!(why("c"), Some(NoiseReason::Volatile { streak: 5 }));
        assert_eq!(s.suppressed.iter().filter(|(_, r)| *r == NoiseReason::RateLimited).count(), 1);
    }

    #[test]
    fn raw_words_become_raw_locations() {
        let mut a = vec![0u8; 64];
        let mut b = a.clone();
        b[8..12].copy_from_slice(&1300i32.to_le_bytes());
        a[8..12].copy_from_slice(&1500i32.to_le_bytes());
        let d = diff(&Node::Bytes(a.clone()), &Node::Bytes(b));
        let s = screen(&d, &State::default(), &WatchOptions::default());
        assert_eq!(s.questions.len(), 1, "{:?}", s.suppressed);
        let q = &s.questions[0];
        assert_eq!(q.old, ScalarValue::Int(1500));
        assert_eq!(q.new, ScalarValue::Int(1300));
        assert!(matches!(q.location, Location::Raw { offset: 8, ty: ScanType::I32, .. }));
        // Many scattered words: bulk.
        let mut c = a.clone();
        for i in 0..10 {
            c[i * 6] ^= 0xff;
        }
        let d = diff(&Node::Bytes(a), &Node::Bytes(c));
        let s = screen(&d, &State::default(), &WatchOptions::default());
        assert!(s.questions.is_empty());
        assert!(matches!(s.suppressed[0].1, NoiseReason::BulkChange { .. }));
    }

    #[test]
    fn default_aliases() {
        let gold = Location::Ast { path: p("player.gold") };
        assert_eq!(default_alias(VariableCategory::Money, &gold), "Money (gold)");
        let money = Location::Ast { path: p("player.money") };
        assert_eq!(default_alias(VariableCategory::Money, &money), "Money");
        let hp = Location::Ast { path: p("Stats.CurrentHP") };
        assert_eq!(default_alias(VariableCategory::Other, &hp), "current hp");
        let raw = Location::Raw { region: NodePath::root(), offset: 8, ty: ScanType::I32, endian: Endianness::Little };
        assert_eq!(default_alias(VariableCategory::Money, &raw), "Money (@0x8)");
    }
}
