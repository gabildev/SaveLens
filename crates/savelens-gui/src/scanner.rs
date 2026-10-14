//! View-models of the Scanner Studio (multi-step differential scans over
//! snapshots of a save) and of the Cheat Table / Pinned Stats panel.

use std::collections::HashMap;

use savelens_cas::GameId;
use savelens_core::scanner::ScanStep;
use savelens_core::{
    Alignment, CandidateView, Endianness, Location, ScalarValue, ScanConfig, ScanFilter, ScanSession, ScanType,
};
use savelens_watcher::{CheatValue, Engine, WORKING};

/// Maximum candidate rows materialised for the results table.
pub const MAX_RESULT_ROWS: usize = 100_000;

/// Filter choices of the filter combo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    Exact,
    IncreasedBy,
    DecreasedBy,
    Increased,
    Decreased,
    Changed,
    Unchanged,
    Between,
    Transition,
    Unknown,
}

impl FilterKind {
    pub const ALL: [FilterKind; 10] = [
        FilterKind::Exact,
        FilterKind::IncreasedBy,
        FilterKind::DecreasedBy,
        FilterKind::Increased,
        FilterKind::Decreased,
        FilterKind::Changed,
        FilterKind::Unchanged,
        FilterKind::Between,
        FilterKind::Transition,
        FilterKind::Unknown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterKind::Exact => "Exact Value",
            FilterKind::IncreasedBy => "Increased by",
            FilterKind::DecreasedBy => "Decreased by",
            FilterKind::Increased => "Increased",
            FilterKind::Decreased => "Decreased",
            FilterKind::Changed => "Changed",
            FilterKind::Unchanged => "Unchanged",
            FilterKind::Between => "Between",
            FilterKind::Transition => "Transition A -> B",
            FilterKind::Unknown => "Unknown initial value",
        }
    }

    /// Number of value inputs the filter needs.
    pub fn value_count(self) -> usize {
        match self {
            FilterKind::Exact | FilterKind::IncreasedBy | FilterKind::DecreasedBy => 1,
            FilterKind::Between | FilterKind::Transition => 2,
            _ => 0,
        }
    }

    /// Valid for a first scan.
    pub fn allowed_first(self) -> bool {
        matches!(self, FilterKind::Exact | FilterKind::Between | FilterKind::Unknown)
    }

    /// Valid for a next scan.
    pub fn allowed_next(self) -> bool {
        self != FilterKind::Unknown
    }
}

/// Offset alignment choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignChoice {
    Unaligned,
    Natural,
    Fixed,
}

impl AlignChoice {
    pub const ALL: [AlignChoice; 3] = [AlignChoice::Unaligned, AlignChoice::Natural, AlignChoice::Fixed];

    pub fn label(self) -> &'static str {
        match self {
            AlignChoice::Unaligned => "Unaligned (every byte)",
            AlignChoice::Natural => "Natural (type width)",
            AlignChoice::Fixed => "Fixed",
        }
    }
}

/// Every scan type offered as a checkbox.
pub const ALL_TYPES: [ScanType; 11] = [
    ScanType::I8,
    ScanType::I16,
    ScanType::I32,
    ScanType::I64,
    ScanType::U8,
    ScanType::U16,
    ScanType::U32,
    ScanType::U64,
    ScanType::F32,
    ScanType::F64,
    ScanType::AsciiDecimal,
];

/// Result of a scan executed in the background.
#[derive(Debug)]
pub struct ScanOutcome {
    pub session: ScanSession,
    pub result: Result<usize, String>,
    pub preview: Vec<CandidateView>,
    pub first: bool,
}

/// Parameters of one scan to run off the UI thread.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub game: GameId,
    pub rel: String,
    pub rev: String,
    pub filter: ScanFilter,
    pub first: bool,
}

/// Runs a scan step (called on a worker thread).
pub fn run_scan(engine: &Engine, req: &ScanRequest, mut session: ScanSession) -> ScanOutcome {
    let label = format!("{}@{}", req.rel, req.rev);
    let result = engine.root_at(req.game, &req.rev, &req.rel).map_err(|e| e.to_string()).and_then(|root| {
        if req.first {
            session.first_scan(&root, &req.filter, &label)
        } else {
            session.next_scan(&root, &req.filter, &label)
        }
        .map_err(|e| e.to_string())
    });
    let preview = session.candidates(0, MAX_RESULT_ROWS);
    ScanOutcome { session, result, preview, first: req.first }
}

/// State of the Scanner Studio.
#[derive(Debug, Clone)]
pub struct ScannerModel {
    /// `WORKING` or a revision (`#12`, `HEAD~1`, a tag, a branch).
    pub source_rev: String,
    pub filter: FilterKind,
    pub value_a: String,
    pub value_b: String,
    pub types: Vec<(ScanType, bool)>,
    pub little_endian: bool,
    pub big_endian: bool,
    pub alignment: AlignChoice,
    pub fixed_alignment: usize,
    pub scan_ast: bool,
    pub scan_numeric_strings: bool,
    pub scan_raw: bool,
    session: Option<ScanSession>,
    results: Vec<CandidateView>,
    total: usize,
    pub error: Option<String>,
    /// File the current session was started on.
    pub scan_file: Option<String>,
}

impl Default for ScannerModel {
    fn default() -> Self {
        let common = ScanType::COMMON;
        ScannerModel {
            source_rev: WORKING.to_string(),
            filter: FilterKind::Exact,
            value_a: String::new(),
            value_b: String::new(),
            types: ALL_TYPES.iter().map(|t| (*t, common.contains(t))).collect(),
            little_endian: true,
            big_endian: false,
            alignment: AlignChoice::Unaligned,
            fixed_alignment: 4,
            scan_ast: true,
            scan_numeric_strings: true,
            scan_raw: true,
            session: None,
            results: Vec::new(),
            total: 0,
            error: None,
            scan_file: None,
        }
    }
}

fn parse_value(label: &str, s: &str) -> Result<ScalarValue, String> {
    ScalarValue::parse(s).map_err(|e| format!("{label}: {e}"))
}

impl ScannerModel {
    /// Builds the filter from the inputs.
    pub fn build_filter(&self) -> Result<ScanFilter, String> {
        let a = || parse_value("value", &self.value_a);
        let b = || parse_value("second value", &self.value_b);
        Ok(match self.filter {
            FilterKind::Exact => ScanFilter::Exact(a()?),
            FilterKind::IncreasedBy => ScanFilter::IncreasedBy(a()?),
            FilterKind::DecreasedBy => ScanFilter::DecreasedBy(a()?),
            FilterKind::Increased => ScanFilter::Increased,
            FilterKind::Decreased => ScanFilter::Decreased,
            FilterKind::Changed => ScanFilter::Changed,
            FilterKind::Unchanged => ScanFilter::Unchanged,
            FilterKind::Between => ScanFilter::Between(a()?, b()?),
            FilterKind::Transition => ScanFilter::Transition { from: a()?, to: b()? },
            FilterKind::Unknown => ScanFilter::Unknown,
        })
    }

    /// Builds the scanner configuration from the checkboxes.
    pub fn build_config(&self) -> ScanConfig {
        let mut endians = Vec::new();
        if self.little_endian {
            endians.push(Endianness::Little);
        }
        if self.big_endian {
            endians.push(Endianness::Big);
        }
        if endians.is_empty() {
            endians.push(Endianness::Little);
        }
        ScanConfig {
            types: self.types.iter().filter(|(_, on)| *on).map(|(t, _)| *t).collect(),
            endians,
            alignment: match self.alignment {
                AlignChoice::Unaligned => Alignment::Unaligned,
                AlignChoice::Natural => Alignment::Natural,
                AlignChoice::Fixed => Alignment::Fixed(self.fixed_alignment.max(1)),
            },
            scan_ast: self.scan_ast,
            scan_numeric_strings: self.scan_numeric_strings,
            scan_raw: self.scan_raw,
            ..ScanConfig::default()
        }
    }

    /// Enables / disables a type.
    pub fn set_type(&mut self, ty: ScanType, on: bool) {
        for (t, enabled) in &mut self.types {
            if *t == ty {
                *enabled = on;
            }
        }
    }

    /// True after a first scan.
    pub fn has_session(&self) -> bool {
        self.session.as_ref().is_some_and(|s| !s.is_fresh())
    }

    /// Prepares a first scan: validates the inputs and creates a fresh session.
    pub fn prepare_first(&mut self, game: GameId, rel: &str) -> Result<(ScanRequest, ScanSession), String> {
        if !self.filter.allowed_first() {
            return Err(format!(
                "`{}` compares with a previous scan; use Exact Value, Between or Unknown initial value for the first scan",
                self.filter.label()
            ));
        }
        if !self.scan_ast && !self.scan_raw {
            return Err("enable AST and/or raw scanning".into());
        }
        let filter = self.build_filter()?;
        let config = self.build_config();
        if config.scan_raw && config.types.is_empty() {
            return Err("select at least one value type".into());
        }
        self.error = None;
        self.scan_file = Some(rel.to_string());
        Ok((
            ScanRequest { game, rel: rel.to_string(), rev: self.source_rev.clone(), filter, first: true },
            ScanSession::new(config),
        ))
    }

    /// Prepares a next scan: takes the session out of the model (it comes back
    /// with [`ScannerModel::finish`]).
    pub fn prepare_next(&mut self, game: GameId) -> Result<(ScanRequest, ScanSession), String> {
        if !self.filter.allowed_next() {
            return Err("`Unknown initial value` is only valid for the first scan".into());
        }
        let filter = self.build_filter()?;
        let rel = self.scan_file.clone().ok_or_else(|| "run a first scan before a next scan".to_string())?;
        if !self.has_session() {
            return Err("run a first scan before a next scan".into());
        }
        let session = self.session.take().ok_or_else(|| "run a first scan before a next scan".to_string())?;
        self.error = None;
        Ok((ScanRequest { game, rel, rev: self.source_rev.clone(), filter, first: false }, session))
    }

    /// Stores the outcome of a background scan.
    pub fn finish(&mut self, out: ScanOutcome) -> Result<usize, String> {
        let first = out.first;
        self.session = Some(out.session);
        match out.result {
            Ok(n) => {
                self.results = out.preview;
                self.total = n;
                self.error = None;
                Ok(n)
            }
            Err(e) => {
                if first {
                    self.results.clear();
                    self.total = 0;
                    self.session = None;
                }
                self.error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Clears the session and the results.
    pub fn reset(&mut self) {
        self.session = None;
        self.results.clear();
        self.total = 0;
        self.error = None;
        self.scan_file = None;
    }

    /// Materialised candidates.
    pub fn results(&self) -> &[CandidateView] {
        &self.results
    }

    /// Total candidate count (including not-yet-materialised raw candidates).
    pub fn total(&self) -> usize {
        self.total
    }

    /// Scan history.
    pub fn history(&self) -> &[ScanStep] {
        self.session.as_ref().map(|s| s.steps()).unwrap_or(&[])
    }
}

/// Default alias of a location (`Gold`, `i32@0x10`), unique among `taken`.
pub fn default_alias(location: &Location, taken: &[String]) -> String {
    let base = match location {
        Location::Ast { path } => {
            let l = path.leaf_label();
            if l.is_empty() {
                "value".to_string()
            } else {
                l
            }
        }
        Location::Raw { offset, ty, .. } => format!("{}@{offset:#x}", ty.name()),
    };
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}_{i}")).find(|c| !taken.contains(c)).unwrap_or(base)
}

/// State of the Cheat Table / Pinned Stats panel.
#[derive(Debug, Clone, Default)]
pub struct CheatTableModel {
    pub rows: Vec<CheatValue>,
    /// Pending value edits by alias (text, parsed on apply).
    pub edits: HashMap<String, String>,
    /// Frozen-value inputs by alias.
    pub freeze_inputs: HashMap<String, String>,
    pub error: Option<String>,
    pub last_safety_tag: Option<String>,
}

impl CheatTableModel {
    /// Replaces the rows, keeping edits of aliases that still exist.
    pub fn set_rows(&mut self, rows: Vec<CheatValue>) {
        self.edits.retain(|a, _| rows.iter().any(|r| r.entry.alias == *a));
        self.freeze_inputs.retain(|a, _| rows.iter().any(|r| r.entry.alias == *a));
        for r in &rows {
            if !self.freeze_inputs.contains_key(&r.entry.alias) {
                let v = r.entry.frozen_value.or(r.current).map(|v| v.to_string()).unwrap_or_default();
                self.freeze_inputs.insert(r.entry.alias.clone(), v);
            }
        }
        self.rows = rows;
    }

    /// Aliases in the table.
    pub fn aliases(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.entry.alias.clone()).collect()
    }

    /// Sets an edited value.
    pub fn set_edit(&mut self, alias: &str, value: impl Into<String>) {
        self.edits.insert(alias.to_string(), value.into());
    }

    /// Edits that differ from the live value, parsed. Errors name the alias.
    pub fn pending_edits(&self) -> Result<Vec<(String, ScalarValue)>, String> {
        let mut out = Vec::new();
        for r in &self.rows {
            let Some(text) = self.edits.get(&r.entry.alias) else { continue };
            if text.trim().is_empty() {
                continue;
            }
            let v = ScalarValue::parse(text).map_err(|e| format!("{}: {e}", r.entry.alias))?;
            if r.current == Some(v) {
                continue;
            }
            out.push((r.entry.alias.clone(), v));
        }
        Ok(out)
    }

    /// Parsed frozen value input of an alias (`None` = use the live value).
    pub fn freeze_value(&self, alias: &str) -> Result<Option<ScalarValue>, String> {
        match self.freeze_inputs.get(alias) {
            Some(t) if !t.trim().is_empty() => ScalarValue::parse(t).map(Some).map_err(|e| format!("{alias}: {e}")),
            _ => Ok(None),
        }
    }
}

/// Sensible slider range for an integer value.
pub fn slider_range(current: i64) -> (i64, i64) {
    if current < 0 {
        let m = current.saturating_mul(4).min(-100);
        (m, -m)
    } else {
        (0, current.saturating_mul(4).max(100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savelens_core::NodePath;

    #[test]
    fn filters_from_inputs() {
        let mut m = ScannerModel { value_a: "1200".into(), ..ScannerModel::default() };
        assert_eq!(m.build_filter(), Ok(ScanFilter::Exact(ScalarValue::Int(1200))));
        m.filter = FilterKind::Transition;
        m.value_b = "1500".into();
        assert_eq!(
            m.build_filter(),
            Ok(ScanFilter::Transition { from: ScalarValue::Int(1200), to: ScalarValue::Int(1500) })
        );
        m.value_b = "abc".into();
        assert!(m.build_filter().is_err());
        assert!(m.prepare_first(1, "a.json").is_err(), "transition is not a first-scan filter");
        m.filter = FilterKind::Changed;
        assert!(m.prepare_next(1).is_err(), "no session yet");
        for k in FilterKind::ALL {
            assert!(!k.label().is_empty());
        }
    }

    #[test]
    fn config_from_checkboxes() {
        let mut m = ScannerModel {
            big_endian: true,
            alignment: AlignChoice::Fixed,
            fixed_alignment: 2,
            ..ScannerModel::default()
        };
        m.set_type(ScanType::U8, true);
        let c = m.build_config();
        assert!(c.types.contains(&ScanType::U8));
        assert_eq!(c.endians, vec![Endianness::Little, Endianness::Big]);
        assert_eq!(c.alignment, Alignment::Fixed(2));
    }

    #[test]
    fn aliases_and_ranges() {
        let loc = Location::Ast { path: NodePath::parse("Player.Gold").unwrap() };
        assert_eq!(default_alias(&loc, &[]), "Gold");
        assert_eq!(default_alias(&loc, &["Gold".into()]), "Gold_2");
        assert_eq!(slider_range(1500), (0, 6000));
        assert_eq!(slider_range(3), (0, 100));
        assert_eq!(slider_range(-50), (-200, 200));
    }
}
