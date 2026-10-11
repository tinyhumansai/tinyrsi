//! Step execution, the environment contract and score parsing.
#![expect(clippy::unwrap_used, reason = "assert deterministic fixtures")]
use super::*;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

fn ctx(root: &Path) -> StepContext {
    StepContext {
        loop_dir: root.to_path_buf(),
        run_dir: root.join("run"),
        incumbent: root.join("inc"),
        epoch: 2,
        model_url: Some("http://127.0.0.1:9/".into()),
        env: BTreeMap::from([("EXTRA".to_owned(), "yes".to_owned())]),
        inherited: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
    }
}

fn step(root: &Path, command: &str) -> StepRun {
    StepRun {
        id: "score.x".into(),
        command: command.into(),
        cwd: root.to_path_buf(),
        candidate: root.join("cand"),
        label: "c1".into(),
        seed: 7,
        dir: root.join("steps/x"),
        timeout: Duration::from_secs(20),
    }
}

fn script(root: &Path, body: &str) -> String {
    let path = root.join("s.sh");
    std::fs::write(&path, body).unwrap();
    format!("/bin/sh {}", path.display())
}

#[test]
fn passes_the_contract_environment() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = script(
        dir.path(),
        "echo \"$RSI_CANDIDATE|$RSI_INCUMBENT|$RSI_EPOCH|$RSI_LABEL|$RSI_SEED|$EXTRA|$RSI_MODEL_URL|$RSI_GATEWAY\"\n\
         test -d \"$RSI_OUT\" && test -d \"$TMPDIR\"\n",
    );
    let run = step(dir.path(), &cmd);
    let outcome = run_step(&ctx(dir.path()), &run);
    assert!(outcome.ok, "{outcome:?}");
    let root = dir.path().display();
    assert_eq!(
        outcome.stdout.trim(),
        format!(
            "{root}/cand|{root}/inc|2|c1|7|yes|http://127.0.0.1:9/__tag/step/score.x/v1|http://127.0.0.1:9"
        )
    );
    assert!(dir.path().join("steps/x/stdout").exists());
}

fn run_step(ctx: &StepContext, step: &StepRun) -> StepOutcome {
    run(ctx, step).unwrap()
}

#[test]
fn reports_the_last_stderr_line_of_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = script(
        dir.path(),
        "echo first >&2\necho 'leaked a canary' >&2\nexit 3\n",
    );
    let outcome = run_step(&ctx(dir.path()), &step(dir.path(), &cmd));
    assert!(!outcome.ok);
    assert_eq!(outcome.reason.as_deref(), Some("leaked a canary"));
    let cmd = script(dir.path(), "exit 4\n");
    let outcome = run_step(&ctx(dir.path()), &step(dir.path(), &cmd));
    assert_eq!(outcome.reason.as_deref(), Some("exited with Some(4)"));
}

#[test]
fn times_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut run = step(dir.path(), &script(dir.path(), "sleep 5\n"));
    run.timeout = Duration::from_millis(100);
    let outcome = run_step(&ctx(dir.path()), &run);
    assert!(!outcome.ok);
    assert!(outcome.reason.unwrap().starts_with("timed out"));
}

#[test]
fn rejects_empty_and_unstartable_commands() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        run(&ctx(dir.path()), &step(dir.path(), "  ")),
        Err(Error::Step { .. })
    ));
    assert!(matches!(
        run(&ctx(dir.path()), &step(dir.path(), "/nonexistent/program")),
        Err(Error::Step { reason, .. }) if reason.contains("could not start")
    ));
}

#[test]
fn reads_result_json_or_a_metric_line() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = script(
        dir.path(),
        "cat > \"$RSI_OUT/result.json\" <<'J'\n\
         {\"score\": 0.5, \"splits\": {\"test\": {\"items\": {\"q1\": 1}}}, \
          \"failures\": [{\"id\": \"q2\", \"type\": \"temporal\", \"text\": \"wrong\"}], \
          \"metrics\": {\"tokens\": 9}, \"extra\": true}\nJ\n",
    );
    let run = step(dir.path(), &cmd);
    let outcome = run_step(&ctx(dir.path()), &run);
    let result = score_result(&run, &outcome).unwrap();
    assert!((result.score - 0.5).abs() < 1e-12);
    assert!((result.splits["test"].items["q1"] - 1.0).abs() < 1e-12);
    assert_eq!(result.failures[0].kind.as_deref(), Some("temporal"));

    let other = tempfile::tempdir().unwrap();
    let cmd = script(
        other.path(),
        "echo 'METRIC colors=9'\necho 'METRIC colors=7.5'\n",
    );
    let run = step(other.path(), &cmd);
    let outcome = run_step(&ctx(other.path()), &run);
    assert!((score_result(&run, &outcome).unwrap().score - 7.5).abs() < 1e-12);
}

#[test]
fn rejects_missing_malformed_and_non_finite_results() {
    let dir = tempfile::tempdir().unwrap();
    let run = step(dir.path(), "true");
    let outcome = StepOutcome {
        id: "score.x".into(),
        ok: true,
        reason: None,
        duration_ms: 0,
        stdout: "nothing".into(),
    };
    assert!(matches!(
        score_result(&run, &outcome),
        Err(Error::Step { .. })
    ));
    let out = PathBuf::from(&run.dir).join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("result.json"), "{\"nope\": 1}").unwrap();
    assert!(
        matches!(score_result(&run, &outcome), Err(Error::Step { reason, .. }) if reason.starts_with("result.json"))
    );
    std::fs::remove_file(out.join("result.json")).unwrap();
    let infinite = StepOutcome {
        stdout: "METRIC colors=inf".into(),
        ..outcome
    };
    assert!(
        matches!(score_result(&run, &infinite), Err(Error::Step { reason, .. }) if reason == "non-finite score")
    );
}

#[test]
fn pins_the_result_stored_form() {
    let result = ScoreResult {
        score: 1.0,
        splits: BTreeMap::from([(
            "dev".to_owned(),
            Split {
                items: BTreeMap::from([("a".to_owned(), 0.0)]),
            },
        )]),
        failures: vec![Failure {
            id: "a".into(),
            kind: Some("k".into()),
            text: "t".into(),
        }],
        metrics: BTreeMap::new(),
    };
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({
            "score": 1.0, "splits": {"dev": {"items": {"a": 0.0}}},
            "failures": [{"id": "a", "type": "k", "text": "t"}], "metrics": {}
        })
    );
}
