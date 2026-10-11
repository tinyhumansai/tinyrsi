//! Score functions and the decision rule.
//!
//! [`score_checkout`] runs every `run` scorer (once per seed for the `seeds`
//! rule, once for `sign`; up to `score.parallel` seeds at a time) and every
//! judge, and [`decide`] compares a
//! candidate's [`Scorecard`] with the incumbent's.
//!
//! - **`seeds`**: per-seed qualities from the deciding scorer's metric, mixed
//!   with judge grades by weight, compared by `tinyrsi_score::decide`. Only
//!   the metric is accept-eligible, so a judge can block or temper an
//!   acceptance but never cause one. The incumbent has no change to judge and
//!   counts as a neutral 0.5 for every judge.
//! - **`sign`**: the exact sign test on the deciding scorer's per-item scores
//!   for the configured split. Judges are recorded but do not vote.
//!
//! Other `run` scorers are recorded and shown, never decided on.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Mutex, PoisonError},
    thread,
    time::Duration,
};

use tinyrsi_score::{
    Assessment, EvalPolicy, Goal, MetricDirection, MetricEvidence, MetricScorer, ScoreDecision,
    Scorer,
};
use tinyrsi_trace::IntervalUnits;

use crate::{
    Error, Result,
    manifest::{DecideTest, ScoreSpec},
    step::{self, StepContext, StepRun},
    subject::Checkout,
};

mod types;
pub use types::{JudgeScore, Scorecard, SeedScore, Verdict};

/// A judge grade for the incumbent, which has no change to grade.
const NEUTRAL: f64 = 0.5;

/// Judges by scorer id.
pub type Judges = BTreeMap<String, Box<dyn Scorer<str> + Send + Sync>>;

/// The seeds a scorer runs on under `spec`.
#[must_use]
pub fn seeds(spec: &ScoreSpec) -> Vec<u64> {
    match spec.decide.test {
        DecideTest::Seeds => spec.decide.seeds.clone(),
        DecideTest::Sign => vec![0],
    }
}

/// Score one checkout.
///
/// `change` is the candidate's diff; judges only run when it is present and
/// non-empty. A failing scorer run is recorded, not returned as an error.
///
/// # Errors
/// Returns an error when a command cannot be started, or a judge fails.
pub fn score_checkout(
    ctx: &StepContext,
    spec: &ScoreSpec,
    checkout: &Checkout,
    dir: &Path,
    judges: &Judges,
    rubrics: &BTreeMap<String, String>,
    change: Option<&str>,
) -> Result<Scorecard> {
    let seeds = seeds(spec);
    let mut runs = BTreeMap::new();
    for scorer in &spec.scorers {
        let Some(command) = &scorer.run else { continue };
        let run = |seed: u64| StepRun {
            id: format!("score.{}", scorer.id),
            command: command.clone(),
            cwd: ctx.loop_dir.clone(),
            candidate: checkout.path.clone(),
            label: checkout.label.clone(),
            seed,
            dir: dir.join(format!("score.{}.{seed}", scorer.id)),
            timeout: Duration::from_secs(scorer.timeout_secs),
        };
        let results = run_seeds(ctx, &seeds, spec.parallel, &run)?;
        runs.insert(scorer.id.clone(), results);
    }
    let mut graded = BTreeMap::new();
    if let Some(change) = change.filter(|c| !c.trim().is_empty()) {
        for scorer in spec.scorers.iter().filter(|s| s.judge.is_some()) {
            let (Some(judge), Some(rubric)) = (judges.get(&scorer.id), rubrics.get(&scorer.id))
            else {
                continue;
            };
            let assessment = judge.assess(change, &Goal::new(rubric.clone()), &[0])?;
            graded.insert(
                scorer.id.clone(),
                JudgeScore {
                    quality: assessment.qualities.first().copied().unwrap_or(NEUTRAL),
                    rationale: assessment.rationale,
                },
            );
        }
    }
    let headline = runs.get(&spec.decide.on).and_then(|r| mean_score(r));
    Ok(Scorecard {
        label: checkout.label.clone(),
        runs,
        judges: graded,
        headline,
    })
}

/// Run one scorer on every seed, at most `parallel` at a time.
///
/// Results come back in the order of `seeds` whatever order they finish in.
/// With `parallel` of 1 (or a single seed) the seeds run one after another on
/// the calling thread, stopping at the first command that cannot be started.
/// Otherwise worker threads take seeds in order; once any command cannot be
/// started no further seeds are begun, the ones already running finish, and
/// the error of the earliest such seed is returned.
fn run_seeds(
    ctx: &StepContext,
    seeds: &[u64],
    parallel: u32,
    run: &(dyn Fn(u64) -> StepRun + Sync),
) -> Result<Vec<SeedScore>> {
    let workers = usize::try_from(parallel)
        .unwrap_or(usize::MAX)
        .clamp(1, seeds.len().max(1));
    if workers == 1 {
        return seeds
            .iter()
            .map(|&seed| run_seed(ctx, &run(seed)))
            .collect();
    }
    // The next seed index and whether a command failed to start. One lock
    // covers both, so no seed is claimed after a failure is recorded.
    let claims = Mutex::new((0_usize, false));
    let claim = || {
        let mut state = claims.lock().unwrap_or_else(PoisonError::into_inner);
        let (next, failed) = &mut *state;
        if *failed || *next >= seeds.len() {
            return None;
        }
        *next += 1;
        Some(*next - 1)
    };
    let slots: Vec<Mutex<Option<Result<SeedScore>>>> =
        seeds.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(i) = claim() {
                    let result = run_seed(ctx, &run(seeds[i]));
                    if result.is_err() {
                        claims.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
                    }
                    *slots[i].lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(PoisonError::into_inner))
        // A seed left unstarted after an earlier failure is never reached:
        // `collect` stops at that earlier `Err`.
        .map_while(|slot| slot)
        .collect()
}

/// Run one seed and read what it reported.
fn run_seed(ctx: &StepContext, run: &StepRun) -> Result<SeedScore> {
    let outcome = step::run(ctx, run)?;
    let (result, reason) = if outcome.ok {
        match step::score_result(run, &outcome) {
            Ok(result) => (Some(result), None),
            Err(e) => (None, Some(e.to_string())),
        }
    } else {
        (None, outcome.reason.clone())
    };
    Ok(SeedScore {
        seed: run.seed,
        ok: result.is_some(),
        reason,
        result,
        duration_ms: outcome.duration_ms,
    })
}

#[expect(clippy::cast_precision_loss, reason = "seed counts are tiny")]
fn mean_score(runs: &[SeedScore]) -> Option<f64> {
    let values: Option<Vec<f64>> = runs
        .iter()
        .map(|r| r.result.as_ref().map(|x| x.score))
        .collect();
    let values = values.filter(|v| !v.is_empty())?;
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// Whether `score` reaches the manifest's target.
#[must_use]
pub fn reaches(spec: &ScoreSpec, score: f64) -> bool {
    spec.decide
        .target
        .is_some_and(|target| MetricScorer::new(spec.decide.direction).reaches(score, target))
}

/// Whether `a` is a better headline than `b`.
#[must_use]
pub fn better(spec: &ScoreSpec, a: f64, b: f64) -> bool {
    let direction = match spec.decide.test {
        DecideTest::Seeds => spec.decide.direction,
        DecideTest::Sign => MetricDirection::Maximize,
    };
    MetricScorer::new(direction).better(a, b)
}

/// Compare a candidate's scorecard with the incumbent's.
///
/// # Errors
/// Returns [`Error::Score`] when the evidence cannot be compared, such as an
/// incumbent with no headline.
pub fn decide(spec: &ScoreSpec, incumbent: &Scorecard, candidate: &Scorecard) -> Result<Verdict> {
    let on = &spec.decide.on;
    if let Some(failed) = candidate
        .runs
        .get(on)
        .and_then(|r| r.iter().find(|s| !s.ok))
    {
        return Ok(Verdict {
            decision: ScoreDecision::Reject,
            estimate: 0.0,
            lower: 0.0,
            upper: 0.0,
            units: IntervalUnits::Quality,
            sign: None,
            reason: format!(
                "seed {} failed: {}",
                failed.seed,
                failed.reason.as_deref().unwrap_or("no score")
            ),
        });
    }
    match spec.decide.test {
        DecideTest::Seeds => decide_seeds(spec, incumbent, candidate),
        DecideTest::Sign => decide_sign(spec, incumbent, candidate),
    }
}

fn values(card: &Scorecard, on: &str) -> Result<Vec<f64>> {
    card.runs
        .get(on)
        .and_then(|runs| {
            runs.iter()
                .map(|r| r.result.as_ref().map(|x| x.score))
                .collect()
        })
        .ok_or(Error::Score(tinyrsi_score::Error::InvalidEvaluation))
}

fn decide_seeds(spec: &ScoreSpec, incumbent: &Scorecard, candidate: &Scorecard) -> Result<Verdict> {
    let d = &spec.decide;
    let metric = MetricScorer::new(d.direction);
    let deciding_weight = spec
        .scorers
        .iter()
        .find(|s| s.id == d.on)
        .map_or(1.0, |s| s.weight);
    let judge_weights: Vec<(&str, f64)> = spec
        .scorers
        .iter()
        .filter(|s| s.judge.is_some())
        .map(|s| (s.id.as_str(), s.weight))
        .collect();
    let assess = |card: &Scorecard| -> Result<Assessment> {
        let raw = values(card, &d.on)?;
        let eligible: Vec<f64> = raw.iter().map(|m| metric.quality(*m)).collect();
        let total = deciding_weight + judge_weights.iter().map(|(_, w)| w).sum::<f64>();
        let judged: f64 = judge_weights
            .iter()
            .map(|(id, w)| w * card.judges.get(*id).map_or(NEUTRAL, |j| j.quality))
            .sum();
        Ok(Assessment {
            qualities: eligible
                .iter()
                .map(|q| (deciding_weight * q + judged) / total)
                .collect(),
            eligible: Some(eligible),
            metric: Some(MetricEvidence {
                values: raw,
                direction: d.direction,
            }),
            rationale: None,
            tokens: 0,
        })
    };
    let policy = EvalPolicy {
        margin: d.margin,
        ..EvalPolicy::default()
    };
    let decision =
        tinyrsi_score::decide(&assess(incumbent)?, &assess(candidate)?, &d.seeds, &policy)?;
    Ok(Verdict {
        decision: decision.decision,
        estimate: decision.estimate,
        lower: decision.lower,
        upper: decision.upper,
        units: IntervalUnits::Metric,
        sign: None,
        reason: format!("paired comparison on {} seeds", d.seeds.len()),
    })
}

fn items(card: &Scorecard, on: &str, split: &str) -> BTreeMap<String, f64> {
    card.runs
        .get(on)
        .and_then(|r| r.first())
        .and_then(|r| r.result.as_ref())
        .and_then(|r| r.splits.get(split))
        .map(|s| s.items.clone())
        .unwrap_or_default()
}

fn decide_sign(spec: &ScoreSpec, incumbent: &Scorecard, candidate: &Scorecard) -> Result<Verdict> {
    let d = &spec.decide;
    let split = d.split.as_deref().unwrap_or_default();
    let test = tinyrsi_score::sign_test(
        &items(incumbent, &d.on, split),
        &items(candidate, &d.on, split),
        d.alpha,
    )?;
    Ok(Verdict {
        decision: test.decision,
        estimate: test.estimate,
        lower: test.lower,
        upper: test.upper,
        units: IntervalUnits::Metric,
        reason: format!(
            "sign test on {}: {} wins, {} losses of {} items, p = {:.3}",
            split, test.wins, test.losses, test.items, test.p
        ),
        sign: Some(test),
    })
}

#[cfg(test)]
mod test;
