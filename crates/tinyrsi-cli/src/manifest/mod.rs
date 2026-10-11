//! The loop folder: `rsi.json`, the brief, the hive package and rubrics.
//!
//! Like the tinyhivemind hive language, [`parse`] takes an in-memory map of
//! relative path to text and opens no files; [`load_dir`] is the one place
//! that reads a folder from disk. Parsing rejects unknown fields and validates
//! every cross-reference, so a loop that passes `tinyrsi check` can only fail
//! at run time for reasons outside the folder.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path},
};

use crate::{Error, Result};

mod types;
pub use types::{
    Budget, Creativity, DEFAULT_MODEL, DEFAULT_OPENROUTER_ENV, DecideSpec, DecideTest, GateStep,
    HiveSpec, KeySpec, Loop, Manifest, ModelPrice, Models, Retreat, SCHEMA, ScoreSpec, ScorerSpec,
    StepCwd, SubjectKind, SubjectSpec, ToolKey, VERSION,
};

/// The one tool capability a key can enable today.
pub const TOOL_WEB_SEARCH: &str = "web_search";

/// The manifest's file name inside a loop folder.
pub const MANIFEST: &str = "rsi.json";

/// Largest file [`load_dir`] reads; larger files are skipped.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// Directories [`load_dir`] never descends into.
const SKIPPED_DIRS: [&str; 5] = ["target", "node_modules", "__pycache__", "runs", ".venv"];

/// Read every text file of a loop folder into a relative-path map.
///
/// Hidden files and folders (including `.env`), build and cache folders,
/// symlinks, files over 1 MiB and non-UTF-8 files are skipped. Paths use `/`.
///
/// # Errors
/// Returns [`Error::Io`] when the folder cannot be read.
pub fn load_dir(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io("read", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("read", dir, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| Error::io("stat", &path, e))?;
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_ref()) {
                walk(root, &path, files)?;
            }
        } else if kind.is_file() {
            let size = entry
                .metadata()
                .map_err(|e| Error::io("stat", &path, e))?
                .len();
            if size > MAX_FILE_BYTES {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| Error::io("read", &path, e))?;
            if let Ok(text) = String::from_utf8(bytes) {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| Error::Path {
                        path: path.display().to_string(),
                    })?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                files.insert(relative, text);
            }
        }
    }
    Ok(())
}

/// Parse and validate a loop folder given as relative path to text.
///
/// # Errors
/// Returns [`Error::MissingFile`] for a missing manifest, brief, hive file or
/// rubric; [`Error::Json`] for malformed or unknown fields; [`Error::Schema`];
/// [`Error::Path`] for unsafe paths; [`Error::Reference`] for unknown seats or
/// scorers; [`Error::Hive`] for an invalid hive package; and [`Error::Field`]
/// for out-of-range values.
pub fn parse(files: &BTreeMap<String, String>) -> Result<Loop> {
    let source = files.get(MANIFEST).ok_or_else(|| Error::MissingFile {
        path: MANIFEST.into(),
    })?;
    let manifest: Manifest = serde_json::from_str(source).map_err(|e| Error::Json {
        message: e.to_string(),
    })?;
    if manifest.schema != SCHEMA || manifest.version != VERSION {
        return Err(Error::Schema {
            schema: manifest.schema,
            version: manifest.version,
        });
    }
    validate_fields(&manifest)?;
    let brief = file(files, &manifest.brief)?.clone();
    let hive = hive_files(files, &manifest.hive.package)?;
    let package = tinyhivemind_lang::parse(&hive)?;
    let seats: BTreeSet<&str> = package
        .manifest
        .seats
        .iter()
        .map(|seat| seat.id.as_str())
        .collect();
    if !seats.contains(manifest.hive.lead.as_str()) {
        return Err(Error::Reference {
            kind: "seat",
            id: manifest.hive.lead.clone(),
        });
    }
    for spec in manifest.keys.values() {
        if let KeySpec::Tool(tool) = spec {
            for seat in tool.seats.iter().flatten() {
                if !seats.contains(seat.as_str()) {
                    return Err(Error::Reference {
                        kind: "seat",
                        id: seat.clone(),
                    });
                }
            }
        }
    }
    let seat_models: BTreeSet<String> = package
        .manifest
        .seats
        .iter()
        .filter_map(|seat| seat.model.clone())
        .collect();
    let mut rubrics = BTreeMap::new();
    for scorer in &manifest.score.scorers {
        if let Some(rubric) = &scorer.judge {
            rubrics.insert(scorer.id.clone(), file(files, rubric)?.clone());
        }
    }
    let parsed = Loop {
        manifest,
        brief,
        hive,
        rubrics,
    };
    for model in parsed.models_used(&seat_models) {
        if !priced(&parsed.manifest.budget, &model) {
            return Err(Error::field(
                format!("budget.prices.{model}"),
                "the model is used but has no price ceiling",
            ));
        }
    }
    Ok(parsed)
}

/// Whether the gateway will allow `model`: built in, or priced in `budget`.
fn priced(budget: &Budget, model: &str) -> bool {
    budget.prices.contains_key(model) || tinyrsi_gateway::PriceTable::standard().get(model).is_ok()
}

fn file<'a>(files: &'a BTreeMap<String, String>, path: &str) -> Result<&'a String> {
    plain(path)?;
    files
        .get(path)
        .ok_or_else(|| Error::MissingFile { path: path.into() })
}

/// The hive package's JSON and Markdown files, relative to `package`.
fn hive_files(files: &BTreeMap<String, String>, package: &str) -> Result<BTreeMap<String, String>> {
    plain(package)?;
    let prefix = format!("{}/", package.trim_end_matches('/'));
    let hive: BTreeMap<String, String> = files
        .iter()
        .filter_map(|(path, text)| {
            let inner = path.strip_prefix(&prefix)?;
            let keep = Path::new(inner)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json") || e.eq_ignore_ascii_case("md"))
                && !inner.eq_ignore_ascii_case("README.md");
            keep.then(|| (inner.to_owned(), text.clone()))
        })
        .collect();
    if hive.is_empty() {
        return Err(Error::MissingFile {
            path: format!("{prefix}hive.json"),
        });
    }
    Ok(hive)
}

/// Reject absolute paths, `..`, and empty paths.
pub(crate) fn plain(path: &str) -> Result<()> {
    let ok = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if ok {
        Ok(())
    } else {
        Err(Error::Path { path: path.into() })
    }
}

/// True for a conventional environment variable name.
pub(crate) fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn validate_fields(m: &Manifest) -> Result<()> {
    let name_ok = !m.name.is_empty()
        && m.name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !name_ok {
        return Err(Error::field("name", "use lowercase letters, digits and -"));
    }
    validate_subject(&m.subject)?;
    if m.hive.wall_secs == 0 || m.hive.turn_secs == 0 {
        return Err(Error::field(
            "hive",
            "wall_secs and turn_secs must be positive",
        ));
    }
    validate_creativity(&m.creativity)?;
    validate_keys(&m.keys)?;
    validate_score(&m.score)?;
    for (i, step) in m.gate.iter().enumerate() {
        if step.run.split_whitespace().next().is_none() || step.timeout_secs == 0 {
            return Err(Error::field(
                format!("gate[{i}]"),
                "needs a command and a positive timeout",
            ));
        }
    }
    let b = &m.budget;
    for (field, value) in [
        ("budget.run_usd", b.run_usd),
        ("budget.total_usd", b.total_usd),
    ] {
        if value.is_some_and(|v| !v.is_finite() || v <= 0.0) {
            return Err(Error::field(field, "must be a positive amount"));
        }
    }
    if !b.score_reserve_usd.is_finite() || b.score_reserve_usd < 0.0 {
        return Err(Error::field(
            "budget.score_reserve_usd",
            "must be zero or more",
        ));
    }
    for (model, price) in &b.prices {
        let ok = [price.prompt_per_mtok, price.completion_per_mtok]
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0)
            && !price.provider.trim().is_empty();
        if !ok {
            return Err(Error::field(
                format!("budget.prices.{model}"),
                "needs non-negative prices and a provider",
            ));
        }
    }
    if m.epochs == 0 || m.patience == 0 {
        return Err(Error::field(
            "epochs",
            "epochs and patience must be positive",
        ));
    }
    for key in m.env.keys() {
        if !env_name(key) || key.starts_with("RSI_") {
            return Err(Error::field(
                format!("env.{key}"),
                "must be an upper-case name not starting with RSI_",
            ));
        }
    }
    Ok(())
}

fn validate_subject(s: &SubjectSpec) -> Result<()> {
    match s.kind {
        SubjectKind::Dir => plain(&s.path)?,
        // A repository usually lives outside the loop folder, so its path may
        // be absolute or climb out with `..`; it is only ever read through git.
        SubjectKind::Git => {
            if s.path.trim().is_empty() {
                return Err(Error::Path {
                    path: s.path.clone(),
                });
            }
            if s.branch.as_deref().is_none_or(str::is_empty) {
                return Err(Error::field("subject.branch", "required for a git subject"));
            }
        }
    }
    if s.editable.is_empty() {
        return Err(Error::field("subject.editable", "list at least one path"));
    }
    for prefix in s.editable.iter().chain(&s.frozen) {
        plain(prefix)?;
    }
    Ok(())
}

fn temperature(field: &str, value: Option<f64>) -> Result<()> {
    if value.is_some_and(|t| !(0.0..=2.0).contains(&t)) {
        return Err(Error::field(field, "must be between 0 and 2"));
    }
    Ok(())
}

fn validate_creativity(c: &Creativity) -> Result<()> {
    temperature("creativity.temperature", c.temperature)?;
    if let Some(r) = &c.retreat {
        temperature("creativity.retreat.temperature", r.temperature)?;
        if r.variations == 0 || r.turns == 0 || r.ideas == 0 {
            return Err(Error::field(
                "creativity.retreat",
                "variations, turns and ideas must be positive",
            ));
        }
        let known = |value: &Option<String>, allowed: &[&str]| {
            value.as_deref().is_none_or(|v| allowed.contains(&v))
        };
        if !known(&r.mode, &["backrooms", "brainstorm", "critic_pair"]) {
            return Err(Error::field(
                "creativity.retreat.mode",
                "use backrooms, brainstorm or critic_pair",
            ));
        }
        if !known(&r.template, &["cli", "fugue", "student"]) {
            return Err(Error::field(
                "creativity.retreat.template",
                "use cli, fugue or student",
            ));
        }
    }
    Ok(())
}

fn validate_keys(keys: &BTreeMap<String, KeySpec>) -> Result<()> {
    for (capability, spec) in keys {
        let (env, provider) = match spec {
            KeySpec::Env(env) => (env, None),
            KeySpec::Tool(tool) => (&tool.env, Some(&tool.provider)),
        };
        if !env_name(env) {
            return Err(Error::field(
                format!("keys.{capability}"),
                "must name an upper-case environment variable",
            ));
        }
        if capability == "openrouter" && provider.is_some() {
            return Err(Error::field(
                "keys.openrouter",
                "takes a variable name only",
            ));
        }
        if provider.is_some_and(|p| p.trim().is_empty()) {
            return Err(Error::field(
                format!("keys.{capability}"),
                "needs a provider",
            ));
        }
        if provider.is_some() && capability != TOOL_WEB_SEARCH {
            return Err(Error::field(
                format!("keys.{capability}"),
                "the only tool capability is web_search",
            ));
        }
    }
    Ok(())
}

fn validate_score(s: &ScoreSpec) -> Result<()> {
    if s.scorers.is_empty() {
        return Err(Error::field("score.scorers", "list at least one scorer"));
    }
    let mut ids = BTreeSet::new();
    for scorer in &s.scorers {
        let field = format!("score.scorers.{}", scorer.id);
        if scorer.id.is_empty() || !ids.insert(scorer.id.as_str()) {
            return Err(Error::field(field, "ids must be unique and non-empty"));
        }
        if scorer.run.is_some() == scorer.judge.is_some() {
            return Err(Error::field(field, "set exactly one of run and judge"));
        }
        if scorer
            .run
            .as_deref()
            .is_some_and(|run| run.split_whitespace().next().is_none())
        {
            return Err(Error::field(field, "run needs a command"));
        }
        if !scorer.weight.is_finite() || scorer.weight <= 0.0 {
            return Err(Error::field(field, "weight must be positive"));
        }
        if scorer.timeout_secs == 0 || scorer.raters == 0 {
            return Err(Error::field(
                field,
                "timeout_secs and raters must be positive",
            ));
        }
    }
    let d = &s.decide;
    let decider = s.scorers.iter().find(|scorer| scorer.id == d.on);
    match decider {
        None => {
            return Err(Error::Reference {
                kind: "scorer",
                id: d.on.clone(),
            });
        }
        Some(scorer) if scorer.run.is_none() => {
            return Err(Error::field("score.decide.on", "must name a run scorer"));
        }
        Some(_) => {}
    }
    match d.test {
        DecideTest::Seeds => {
            let distinct: BTreeSet<_> = d.seeds.iter().collect();
            if distinct.len() < 2 || distinct.len() != d.seeds.len() {
                return Err(Error::field(
                    "score.decide.seeds",
                    "list at least two distinct seeds",
                ));
            }
        }
        DecideTest::Sign => {
            if d.split.as_deref().is_none_or(str::is_empty) {
                return Err(Error::field(
                    "score.decide.split",
                    "required for the sign test",
                ));
            }
        }
    }
    if !(d.alpha > 0.0 && d.alpha < 1.0) {
        return Err(Error::field(
            "score.decide.alpha",
            "must be between 0 and 1",
        ));
    }
    if !d.margin.is_finite() || d.margin < 0.0 {
        return Err(Error::field("score.decide.margin", "must be zero or more"));
    }
    if d.target.is_some_and(|t| !t.is_finite()) {
        return Err(Error::field("score.decide.target", "must be finite"));
    }
    Ok(())
}

impl Loop {
    /// The model for seats without their own.
    #[must_use]
    pub fn seat_model(&self) -> &str {
        self.manifest
            .models
            .default
            .as_deref()
            .unwrap_or(DEFAULT_MODEL)
    }

    /// The model judge scorers use.
    #[must_use]
    pub fn judge_model(&self) -> &str {
        self.manifest
            .models
            .judge
            .as_deref()
            .unwrap_or_else(|| self.seat_model())
    }

    /// The model the creative retreat uses.
    #[must_use]
    pub fn retreat_model(&self) -> &str {
        self.manifest
            .creativity
            .retreat
            .as_ref()
            .and_then(|r| r.model.as_deref())
            .or(self.manifest.models.retreat.as_deref())
            .unwrap_or_else(|| self.seat_model())
    }

    /// The variable holding the `OpenRouter` key.
    #[must_use]
    pub fn openrouter_env(&self) -> &str {
        match self.manifest.keys.get("openrouter") {
            Some(KeySpec::Env(env)) => env,
            _ => DEFAULT_OPENROUTER_ENV,
        }
    }

    /// Tool capabilities, by capability name.
    pub fn tool_keys(&self) -> impl Iterator<Item = (&str, &ToolKey)> {
        self.manifest
            .keys
            .iter()
            .filter_map(|(name, spec)| match spec {
                KeySpec::Tool(tool) => Some((name.as_str(), tool)),
                KeySpec::Env(_) => None,
            })
    }

    /// Every chat model the loop will call: seats, judges and the retreat.
    #[must_use]
    pub fn models_used(&self, seat_models: &BTreeSet<String>) -> BTreeSet<String> {
        let mut models = seat_models.clone();
        models.insert(self.seat_model().to_owned());
        if !self.rubrics.is_empty() {
            models.insert(self.judge_model().to_owned());
        }
        if self
            .manifest
            .creativity
            .retreat
            .as_ref()
            .is_some_and(|r| r.enabled)
        {
            models.insert(self.retreat_model().to_owned());
        }
        models
    }

    /// Every environment variable the loop needs, sorted and unique.
    #[must_use]
    pub fn required_env(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        names.insert(self.openrouter_env().to_owned());
        for spec in self.manifest.keys.values() {
            match spec {
                KeySpec::Env(env) => names.insert(env.clone()),
                KeySpec::Tool(tool) => names.insert(tool.env.clone()),
            };
        }
        names.into_iter().collect()
    }
}

#[cfg(test)]
mod test;
