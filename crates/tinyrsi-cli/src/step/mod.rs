//! Run one gate or scorer command under the step contract.
//!
//! The command runs through the bounded process runner from
//! `tinyrsi-verify`: its own process group, stdin closed, a wall-clock limit,
//! an output cap, and an environment holding only `PATH`, `HOME`, `TMPDIR`,
//! the manifest's `env` and the `RSI_*` variables. Its `stdout` and `stderr`
//! stay in the invocation folder for later inspection.

use std::{ffi::OsStr, fs, path::Path};

use tinyrsi_verify::process::{self, Exit, ProcessSpec};

use crate::{Error, Result};

mod types;
pub use types::{Failure, ScoreResult, Split, StepContext, StepOutcome, StepRun};

/// Combined stdout and stderr limit for one invocation.
const OUTPUT_CAP: u64 = 16 << 20;

/// Run one invocation.
///
/// # Errors
/// Returns [`Error::Step`] for an empty command, [`Error::Io`] when the
/// invocation folder cannot be created, and [`Error::Process`] when the
/// command cannot be started.
pub fn run(ctx: &StepContext, step: &StepRun) -> Result<StepOutcome> {
    let mut words = step.command.split_whitespace();
    let program = words.next().ok_or_else(|| Error::Step {
        step: step.id.clone(),
        reason: "empty command".into(),
    })?;
    let args: Vec<String> = words.map(str::to_owned).collect();
    let out = step.dir.join("out");
    let tmp = step.dir.join("tmp");
    for dir in [&out, &tmp] {
        fs::create_dir_all(dir).map_err(|e| Error::io("create", dir, e))?;
    }
    let env = environment(ctx, step, &out, &tmp);
    let outcome = process::run(&ProcessSpec {
        program: OsStr::new(program),
        args: &args,
        cwd: &step.cwd,
        scratch: &step.dir,
        env: &env,
        timeout: step.timeout,
        cap: OUTPUT_CAP,
    })
    .map_err(|_| Error::Step {
        step: step.id.clone(),
        reason: format!("could not start {program}"),
    })?;
    let stdout = String::from_utf8_lossy(&outcome.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    let (ok, reason) = match outcome.exit {
        Exit::Exited { success: true, .. } => (true, None),
        Exit::Exited { code, .. } => (
            false,
            Some(last_line(&stderr).map_or_else(|| format!("exited with {code:?}"), str::to_owned)),
        ),
        Exit::TimedOut => (false, Some(format!("timed out after {:?}", step.timeout))),
        Exit::OutputExceeded => (false, Some("output exceeded 16 MiB".into())),
    };
    Ok(StepOutcome {
        id: step.id.clone(),
        ok,
        reason,
        duration_ms: outcome.duration_ms,
        stdout,
    })
}

fn environment(ctx: &StepContext, step: &StepRun, out: &Path, tmp: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ctx.inherited.clone();
    env.extend(ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let path = |p: &Path| p.display().to_string();
    env.extend([
        ("TMPDIR".to_owned(), path(tmp)),
        ("RSI_CANDIDATE".to_owned(), path(&step.candidate)),
        ("RSI_INCUMBENT".to_owned(), path(&ctx.incumbent)),
        ("RSI_OUT".to_owned(), path(out)),
        ("RSI_RUN_DIR".to_owned(), path(&ctx.run_dir)),
        ("RSI_LOOP_DIR".to_owned(), path(&ctx.loop_dir)),
        ("RSI_EPOCH".to_owned(), ctx.epoch.to_string()),
        ("RSI_LABEL".to_owned(), step.label.clone()),
        ("RSI_SEED".to_owned(), step.seed.to_string()),
    ]);
    if let Some(base) = &ctx.model_url {
        env.push((
            "RSI_GATEWAY".to_owned(),
            base.trim_end_matches('/').to_owned(),
        ));
        env.push((
            "RSI_MODEL_URL".to_owned(),
            format!(
                "{}/__tag/{}/{}/v1",
                base.trim_end_matches('/'),
                "step",
                step.id
            ),
        ));
    }
    env
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// Read what a scorer reported: `out/result.json`, else the last
/// `METRIC <name>=<float>` line of stdout.
///
/// # Errors
/// Returns [`Error::Step`] when there is neither, when `result.json` does not
/// decode, or when any reported number is not finite.
pub fn score_result(step: &StepRun, outcome: &StepOutcome) -> Result<ScoreResult> {
    let fail = |reason: String| Error::Step {
        step: step.id.clone(),
        reason,
    };
    let path = step.dir.join("out/result.json");
    let result = if let Ok(text) = fs::read_to_string(&path) {
        serde_json::from_str::<ScoreResult>(&text).map_err(|e| fail(format!("result.json: {e}")))?
    } else {
        let score = outcome
            .stdout
            .lines()
            .rev()
            .find_map(|line| {
                let rest = line.trim().strip_prefix("METRIC ")?;
                rest.split_once('=')?.1.trim().parse::<f64>().ok()
            })
            .ok_or_else(|| fail("no result.json and no METRIC line".into()))?;
        ScoreResult {
            score,
            ..ScoreResult::default()
        }
    };
    let finite = result.score.is_finite()
        && result.metrics.values().all(|v| v.is_finite())
        && result
            .splits
            .values()
            .all(|s| s.items.values().all(|v| v.is_finite()));
    if finite {
        Ok(result)
    } else {
        Err(fail("non-finite score".into()))
    }
}

#[cfg(test)]
mod test;
