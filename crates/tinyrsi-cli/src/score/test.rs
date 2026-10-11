//! Scoring checkouts, judges, and both decision rules.
#![expect(clippy::unwrap_used, reason = "assert deterministic fixtures")]
use super::*;
use crate::{
    manifest::{DecideSpec, ScorerSpec},
    step::{ScoreResult, Split},
};
use tinyrsi_score::ScriptedScorer;

fn scorer(id: &str, run: Option<&str>, judge: Option<&str>, weight: f64) -> ScorerSpec {
    ScorerSpec {
        id: id.into(),
        run: run.map(str::to_owned),
        judge: judge.map(str::to_owned),
        weight,
        timeout_secs: 20,
        raters: 1,
    }
}

fn seeds_spec(direction: MetricDirection, scorers: Vec<ScorerSpec>) -> ScoreSpec {
    ScoreSpec {
        scorers,
        parallel: 1,
        decide: DecideSpec {
            on: "m".into(),
            test: DecideTest::Seeds,
            seeds: vec![1, 2, 3],
            split: None,
            alpha: 0.1,
            direction,
            margin: 0.0,
            target: Some(6.0),
        },
    }
}

fn card(label: &str, scores: &[f64]) -> Scorecard {
    Scorecard {
        label: label.into(),
        runs: BTreeMap::from([(
            "m".to_owned(),
            scores
                .iter()
                .enumerate()
                .map(|(i, s)| SeedScore {
                    seed: i as u64 + 1,
                    ok: true,
                    reason: None,
                    result: Some(ScoreResult {
                        score: *s,
                        ..ScoreResult::default()
                    }),
                    duration_ms: 1,
                })
                .collect(),
        )]),
        judges: BTreeMap::new(),
        headline: Some(
            scores.iter().sum::<f64>() / f64::from(u32::try_from(scores.len()).unwrap()),
        ),
    }
}

fn ctx(root: &Path) -> StepContext {
    StepContext {
        loop_dir: root.to_path_buf(),
        run_dir: root.to_path_buf(),
        incumbent: root.to_path_buf(),
        epoch: 0,
        model_url: None,
        env: BTreeMap::new(),
        inherited: vec![("PATH".into(), "/usr/bin:/bin".into())],
    }
}

#[test]
fn runs_each_scorer_per_seed_and_records_failures() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("m.sh"), "echo \"METRIC m=$RSI_SEED\"\n").unwrap();
    std::fs::write(dir.path().join("bad.sh"), "echo nope >&2; exit 1\n").unwrap();
    let spec = seeds_spec(
        MetricDirection::Minimize,
        vec![
            scorer("m", Some("/bin/sh m.sh"), None, 1.0),
            scorer("bad", Some("/bin/sh bad.sh"), None, 1.0),
            scorer("q", None, Some("rubric.md"), 0.5),
        ],
    );
    let checkout = Checkout {
        label: "incumbent".into(),
        path: dir.path().to_path_buf(),
    };
    let card = score_checkout(
        &ctx(dir.path()),
        &spec,
        &checkout,
        &dir.path().join("s"),
        &Judges::new(),
        &BTreeMap::new(),
        None,
    )
    .unwrap();
    assert_eq!(card.runs["m"].len(), 3);
    assert_eq!(card.headline, Some(2.0));
    assert!(
        card.runs["bad"]
            .iter()
            .all(|r| !r.ok && r.reason.as_deref() == Some("nope"))
    );
    assert!(card.judges.is_empty());
}

#[test]
fn judges_grade_a_non_empty_change() {
    let dir = tempfile::tempdir().unwrap();
    let spec = seeds_spec(
        MetricDirection::Minimize,
        vec![
            scorer("q", None, Some("r.md"), 1.0),
            scorer("m", Some("true"), None, 1.0),
        ],
    );
    let mut judges = Judges::new();
    judges.insert(
        "q".into(),
        Box::new(ScriptedScorer::uniform(|change: &str| {
            if change.contains("good") { 0.9 } else { 0.1 }
        })),
    );
    let rubrics = BTreeMap::from([("q".to_owned(), "Is the change good?".to_owned())]);
    let checkout = Checkout {
        label: "c".into(),
        path: dir.path().to_path_buf(),
    };
    let card = score_checkout(
        &ctx(dir.path()),
        &spec,
        &checkout,
        &dir.path().join("s"),
        &judges,
        &rubrics,
        Some("+good"),
    )
    .unwrap();
    assert!((card.judges["q"].quality - 0.9).abs() < 1e-12);
    assert_eq!(card.headline, None, "true prints no metric");
    let none = score_checkout(
        &ctx(dir.path()),
        &spec,
        &checkout,
        &dir.path().join("t"),
        &judges,
        &rubrics,
        Some("  "),
    )
    .unwrap();
    assert!(none.judges.is_empty());
}

#[test]
fn seeds_rule_accepts_a_consistent_improvement_and_rejects_a_failed_seed() {
    let spec = seeds_spec(
        MetricDirection::Minimize,
        vec![scorer("m", Some("x"), None, 1.0)],
    );
    let v = decide(
        &spec,
        &card("incumbent", &[9.0, 9.0, 9.0]),
        &card("c", &[7.0, 7.0, 7.1]),
    )
    .unwrap();
    assert_eq!(v.decision, ScoreDecision::Accept);
    assert!(v.estimate > 1.9);
    let v = decide(
        &spec,
        &card("incumbent", &[7.0, 7.0, 7.0]),
        &card("c", &[9.0, 9.0, 9.0]),
    )
    .unwrap();
    assert_eq!(v.decision, ScoreDecision::Reject);

    let mut failed = card("c", &[1.0, 1.0, 1.0]);
    failed.runs.get_mut("m").unwrap()[1] = SeedScore {
        seed: 2,
        ok: false,
        reason: Some("timed out".into()),
        result: None,
        duration_ms: 0,
    };
    let v = decide(&spec, &card("incumbent", &[9.0, 9.0, 9.0]), &failed).unwrap();
    assert_eq!(v.decision, ScoreDecision::Reject);
    assert_eq!(v.reason, "seed 2 failed: timed out");
}

#[test]
fn a_judge_cannot_accept_alone_but_can_temper() {
    let spec = seeds_spec(
        MetricDirection::Minimize,
        vec![
            scorer("m", Some("x"), None, 1.0),
            scorer("q", None, Some("r"), 3.0),
        ],
    );
    let mut flat = card("c", &[9.0, 9.0, 9.0]);
    flat.judges.insert(
        "q".into(),
        JudgeScore {
            quality: 1.0,
            rationale: None,
        },
    );
    let v = decide(&spec, &card("incumbent", &[9.0, 9.0, 9.0]), &flat).unwrap();
    assert_ne!(v.decision, ScoreDecision::Accept);
}

#[test]
fn an_incumbent_without_scores_cannot_be_compared() {
    let spec = seeds_spec(
        MetricDirection::Minimize,
        vec![scorer("m", Some("x"), None, 1.0)],
    );
    let mut empty = card("incumbent", &[1.0]);
    empty.runs.clear();
    assert!(matches!(
        decide(&spec, &empty, &card("c", &[1.0, 1.0, 1.0])),
        Err(Error::Score(_))
    ));
}

fn sign_card(label: &str, items: &[(String, f64)]) -> Scorecard {
    let result = ScoreResult {
        score: 0.0,
        splits: BTreeMap::from([(
            "test".to_owned(),
            Split {
                items: items.iter().cloned().collect(),
            },
        )]),
        ..ScoreResult::default()
    };
    Scorecard {
        label: label.into(),
        runs: BTreeMap::from([(
            "m".to_owned(),
            vec![SeedScore {
                seed: 0,
                ok: true,
                reason: None,
                result: Some(result),
                duration_ms: 0,
            }],
        )]),
        judges: BTreeMap::new(),
        headline: Some(0.0),
    }
}

#[test]
fn sign_rule_uses_the_split_items() {
    let mut spec = seeds_spec(
        MetricDirection::Maximize,
        vec![scorer("m", Some("x"), None, 1.0)],
    );
    spec.decide.test = DecideTest::Sign;
    spec.decide.split = Some("test".into());
    assert_eq!(seeds(&spec), vec![0]);
    let inc: Vec<_> = (0..20).map(|i| (format!("q{i}"), 0.0)).collect();
    let cand: Vec<_> = (0..20)
        .map(|i| (format!("q{i}"), if i < 8 { 1.0 } else { 0.0 }))
        .collect();
    let v = decide(&spec, &sign_card("incumbent", &inc), &sign_card("c", &cand)).unwrap();
    assert_eq!(v.decision, ScoreDecision::Accept);
    assert_eq!(v.sign.unwrap().wins, 8);
    assert!(v.reason.starts_with("sign test on test: 8 wins"));
}

#[test]
fn targets_and_ordering_follow_the_direction() {
    let spec = seeds_spec(MetricDirection::Minimize, vec![]);
    assert!(reaches(&spec, 6.0) && !reaches(&spec, 6.5));
    assert!(better(&spec, 6.0, 7.0));
    let mut sign = spec.clone();
    sign.decide.test = DecideTest::Sign;
    assert!(better(&sign, 0.7, 0.6));
}

#[test]
fn pins_the_scorecard_stored_form() {
    let json = serde_json::to_value(card("c", &[1.0])).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "label": "c",
            "runs": {"m": [{"seed": 1, "ok": true, "reason": null, "duration_ms": 1,
                "result": {"score": 1.0, "splits": {}, "failures": [], "metrics": {}, "private": {}}}]},
            "judges": {}, "headline": 1.0
        })
    );
}

/// Score `dir` as the incumbent with one `run` scorer `m` running `script`
/// under `parallel`.
fn score_script(dir: &Path, script: &str, parallel: u32) -> Result<Scorecard> {
    std::fs::write(dir.join("m.sh"), script).unwrap();
    let mut spec = seeds_spec(
        MetricDirection::Minimize,
        vec![scorer("m", Some("/bin/sh m.sh"), None, 1.0)],
    );
    spec.parallel = parallel;
    let checkout = Checkout {
        label: "incumbent".into(),
        path: dir.to_path_buf(),
    };
    score_checkout(
        &ctx(dir),
        &spec,
        &checkout,
        &dir.join(format!("s{parallel}")),
        &Judges::new(),
        &BTreeMap::new(),
        None,
    )
}

/// A scorer that waits until `$BARRIER` seeds have started (failing after
/// ten seconds), then reports how many seeds were running at that moment.
///
/// Seeds that overlap pass the barrier; seeds run one at a time can only pass
/// a barrier of 1. The reported peak bounds how many ran at once, so neither
/// check depends on how fast the machine is.
fn barrier(seeds: u32) -> String {
    format!(
        "mkdir -p started running\n\
         touch started/$RSI_SEED running/$RSI_SEED\n\
         i=0\n\
         while [ $(( $(ls started | wc -l) )) -lt {seeds} ]; do\n\
         \x20 i=$((i + 1))\n\
         \x20 if [ $i -gt 200 ]; then rm running/$RSI_SEED; echo barrier >&2; exit 1; fi\n\
         \x20 sleep 0.05\n\
         done\n\
         peak=$(( $(ls running | wc -l) ))\n\
         sleep 0.2\n\
         rm running/$RSI_SEED\n\
         echo \"METRIC m=$peak\"\n"
    )
}

/// The peak each seed reported, in result order, or `None` for a failed seed.
fn peaks(card: &Scorecard) -> Vec<(u64, Option<f64>)> {
    card.runs["m"]
        .iter()
        .map(|r| (r.seed, r.result.as_ref().map(|x| x.score)))
        .collect()
}

#[test]
fn seeds_run_one_at_a_time_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let card = score_script(dir.path(), &barrier(1), 1);
    let card = card.unwrap();
    assert_eq!(
        peaks(&card),
        vec![(1, Some(1.0)), (2, Some(1.0)), (3, Some(1.0))],
        "never more than one seed running"
    );
}

#[test]
fn parallel_seeds_run_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let card = score_script(dir.path(), &barrier(3), 3);
    let card = card.unwrap();
    assert!(
        card.runs["m"].iter().all(|r| r.ok),
        "all three seeds met at the barrier: {:?}",
        card.runs["m"]
    );
    assert_eq!(
        peaks(&card).iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn parallel_limit_caps_concurrent_seeds() {
    let dir = tempfile::tempdir().unwrap();
    let card = score_script(dir.path(), &barrier(2), 2);
    let card = card.unwrap();
    assert!(card.runs["m"].iter().all(|r| r.ok), "two seeds overlapped");
    assert!(
        peaks(&card)
            .iter()
            .all(|(_, p)| p.is_some_and(|p| p <= 2.0)),
        "never more than two running: {:?}",
        peaks(&card)
    );
}

#[test]
fn parallel_seeds_keep_seed_order_and_report_failures_like_sequential_runs() {
    // Seed 1 finishes last and seed 2 fails, so completion order differs
    // from seed order.
    let script = "case $RSI_SEED in\n  1) sleep 1 ;;\n  2) echo broken >&2; exit 3 ;;\nesac\necho \"METRIC m=$RSI_SEED\"\n";
    let dir = tempfile::tempdir().unwrap();
    let sequential = score_script(dir.path(), script, 1);
    let parallel = score_script(dir.path(), script, 3);
    let strip = |card: Scorecard| -> Vec<(u64, bool, Option<String>, Option<f64>)> {
        card.runs["m"]
            .iter()
            .map(|r| {
                (
                    r.seed,
                    r.ok,
                    r.reason.clone(),
                    r.result.as_ref().map(|x| x.score),
                )
            })
            .collect()
    };
    let expected = vec![
        (1, true, None, Some(1.0)),
        (2, false, Some("broken".to_owned()), None),
        (3, true, None, Some(3.0)),
    ];
    let parallel = parallel.unwrap();
    assert_eq!(parallel.headline, None, "a failed seed leaves no headline");
    assert_eq!(strip(sequential.unwrap()), expected);
    assert_eq!(strip(parallel), expected);
    for seed in 1..=3 {
        assert!(dir.path().join(format!("s3/score.m.{seed}/out")).is_dir());
    }
}

#[test]
fn parallel_seeds_keep_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = seeds_spec(
        MetricDirection::Minimize,
        vec![scorer("m", Some("/bin/sh m.sh"), None, 1.0)],
    );
    spec.scorers[0].timeout_secs = 1;
    spec.parallel = 3;
    std::fs::write(dir.path().join("m.sh"), "sleep 30\n").unwrap();
    let checkout = Checkout {
        label: "incumbent".into(),
        path: dir.path().to_path_buf(),
    };
    let start = std::time::Instant::now();
    let card = score_checkout(
        &ctx(dir.path()),
        &spec,
        &checkout,
        &dir.path().join("s"),
        &Judges::new(),
        &BTreeMap::new(),
        None,
    )
    .unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert!(card.runs["m"].iter().all(|r| {
        !r.ok
            && r.reason
                .as_deref()
                .is_some_and(|x| x.starts_with("timed out"))
    }));
}

#[test]
fn an_unstartable_scorer_fails_the_checkout_in_parallel_too() {
    for parallel in [1, 3] {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = seeds_spec(
            MetricDirection::Minimize,
            vec![scorer("m", Some("/nonexistent/scorer"), None, 1.0)],
        );
        spec.parallel = parallel;
        let checkout = Checkout {
            label: "incumbent".into(),
            path: dir.path().to_path_buf(),
        };
        let result = score_checkout(
            &ctx(dir.path()),
            &spec,
            &checkout,
            &dir.path().join("s"),
            &Judges::new(),
            &BTreeMap::new(),
            None,
        );
        assert!(
            matches!(&result, Err(Error::Step { step, .. }) if step == "score.m"),
            "parallel {parallel}: {result:?}"
        );
    }
}
