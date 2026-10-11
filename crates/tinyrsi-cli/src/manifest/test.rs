//! Parsing, validation and the stored form of `rsi.json`.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "assert deterministic fixtures"
)]
use super::*;
use serde_json::{Value, json};

/// An edit applied to a parsed `rsi.json`.
type Edit = fn(&mut Value);

fn example_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops/graph-coloring")
}

fn example() -> BTreeMap<String, String> {
    load_dir(&example_dir()).unwrap()
}

/// The example folder with `rsi.json` edited by `edit`.
fn with(edit: impl FnOnce(&mut Value)) -> BTreeMap<String, String> {
    let mut files = example();
    let mut manifest: Value = serde_json::from_str(&files[MANIFEST]).unwrap();
    edit(&mut manifest);
    files.insert(MANIFEST.into(), manifest.to_string());
    files
}

fn field_error(files: &BTreeMap<String, String>) -> String {
    match parse(files) {
        Err(Error::Field { field, .. }) => field,
        other => panic!("expected a field error, got {other:?}"),
    }
}

#[test]
fn parses_the_graph_coloring_example() {
    let parsed = parse(&example()).unwrap();
    assert_eq!(parsed.manifest.name, "graph-coloring");
    assert_eq!(parsed.manifest.subject.kind, SubjectKind::Dir);
    assert_eq!(parsed.manifest.epochs, 3);
    assert_eq!(parsed.manifest.patience, 2);
    assert!(parsed.brief.contains("{{incumbent}}"));
    assert!(parsed.hive.contains_key("hive.json"));
    assert!(parsed.hive.contains_key("seats/lead.md"));
    assert!(parsed.rubrics.is_empty());
    assert_eq!(parsed.seat_model(), DEFAULT_MODEL);
    assert_eq!(parsed.judge_model(), DEFAULT_MODEL);
    assert_eq!(parsed.retreat_model(), DEFAULT_MODEL);
    assert_eq!(
        parsed.required_env(),
        vec![DEFAULT_OPENROUTER_ENV.to_owned()]
    );
}

#[test]
fn load_dir_skips_hidden_files_caches_and_binary_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
    fs::create_dir_all(dir.path().join("__pycache__")).unwrap();
    fs::write(dir.path().join("__pycache__/x.pyc"), "x").unwrap();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/c.md"), "text").unwrap();
    fs::write(dir.path().join("blob"), [0xff, 0xfe]).unwrap();
    let files = load_dir(dir.path()).unwrap();
    assert_eq!(files.keys().collect::<Vec<_>>(), vec!["a/b/c.md"]);
}

#[test]
fn load_dir_reports_a_missing_folder() {
    assert!(matches!(
        load_dir(Path::new("/nonexistent/tinyrsi-loop")),
        Err(Error::Io { .. })
    ));
}

#[test]
fn rejects_a_missing_manifest_brief_hive_or_rubric() {
    let mut files = example();
    files.remove(MANIFEST);
    assert!(matches!(parse(&files), Err(Error::MissingFile { path }) if path == MANIFEST));

    let mut files = example();
    files.remove("brief.md");
    assert!(matches!(parse(&files), Err(Error::MissingFile { path }) if path == "brief.md"));

    let files = with(|m| m["hive"]["package"] = json!("nohive"));
    assert!(
        matches!(parse(&files), Err(Error::MissingFile { path }) if path == "nohive/hive.json")
    );

    let files = with(|m| {
        m["score"]["scorers"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "q", "judge": "scorers/quality.md" }));
    });
    assert!(
        matches!(parse(&files), Err(Error::MissingFile { path }) if path == "scorers/quality.md")
    );
}

#[test]
fn rejects_unknown_fields_and_malformed_json() {
    let files = with(|m| m["surprise"] = json!(1));
    assert!(matches!(parse(&files), Err(Error::Json { message }) if message.contains("surprise")));
    let files = with(|m| m["subject"]["kind"] = json!("svn"));
    assert!(matches!(parse(&files), Err(Error::Json { .. })));
}

#[test]
fn rejects_an_unsupported_schema_or_version() {
    let files = with(|m| m["version"] = json!(2));
    assert!(matches!(
        parse(&files),
        Err(Error::Schema { version: 2, .. })
    ));
    let files = with(|m| m["$schema"] = json!("https://example.com"));
    assert!(matches!(parse(&files), Err(Error::Schema { .. })));
}

#[test]
fn rejects_unsafe_paths() {
    for edit in [
        (|m: &mut Value| m["brief"] = json!("../brief.md")) as fn(&mut Value),
        |m| m["brief"] = json!("/etc/passwd"),
        |m| m["subject"]["path"] = json!("../elsewhere"),
        |m| m["subject"]["editable"] = json!(["../x"]),
        |m| m["hive"]["package"] = json!(""),
    ] {
        assert!(matches!(parse(&with(edit)), Err(Error::Path { .. })));
    }
    let files = with(|m| {
        m["subject"] =
            json!({ "kind": "git", "path": " ", "branch": "main", "editable": ["src/"] });
    });
    assert!(matches!(parse(&files), Err(Error::Path { .. })));
    // A repository may sit outside the loop folder.
    let files = with(|m| {
        m["subject"] = json!({ "kind": "git", "path": "../../vendor/repo", "branch": "main", "editable": ["src/"] });
    });
    assert!(parse(&files).is_ok());
}

#[test]
fn accepts_a_git_subject_with_an_absolute_path_and_a_branch() {
    let files = with(|m| {
        m["subject"] = json!({
            "kind": "git", "path": "/abs/repo", "branch": "rsi/main",
            "editable": ["src/"], "frozen": ["src/api/"], "require_test_change": true
        });
    });
    let parsed = parse(&files).unwrap();
    assert_eq!(parsed.manifest.subject.branch.as_deref(), Some("rsi/main"));
    let files = with(|m| {
        m["subject"] = json!({ "kind": "git", "path": "/abs/repo", "editable": ["src/"] });
    });
    assert_eq!(field_error(&files), "subject.branch");
}

#[test]
fn rejects_unknown_seats_and_scorers() {
    let files = with(|m| m["hive"]["lead"] = json!("ghost"));
    assert!(matches!(parse(&files), Err(Error::Reference { kind: "seat", id }) if id == "ghost"));
    let files = with(|m| {
        m["keys"] = json!({ "web_search": { "provider": "gemini", "env": "GEMINI_API_KEY", "seats": ["ghost"] } });
    });
    assert!(matches!(
        parse(&files),
        Err(Error::Reference { kind: "seat", .. })
    ));
    let files = with(|m| m["score"]["decide"]["on"] = json!("ghost"));
    assert!(matches!(
        parse(&files),
        Err(Error::Reference { kind: "scorer", .. })
    ));
}

#[test]
fn rejects_an_invalid_hive_package() {
    let mut files = example();
    files.insert("hive/hive.json".into(), "{ not json".into());
    assert!(matches!(parse(&files), Err(Error::Hive(_))));
}

#[test]
fn rejects_out_of_range_fields() {
    let cases: Vec<(&str, Edit)> = vec![
        ("name", |m| m["name"] = json!("Graph Coloring")),
        ("subject.editable", |m| m["subject"]["editable"] = json!([])),
        ("hive", |m| m["hive"]["wall_secs"] = json!(0)),
        ("creativity.temperature", |m| {
            m["creativity"]["temperature"] = json!(3.0);
        }),
        ("creativity.retreat.temperature", |m| {
            m["creativity"]["retreat"] = json!({ "temperature": -1.0 });
        }),
        ("creativity.retreat", |m| {
            m["creativity"]["retreat"] = json!({ "turns": 0 });
        }),
        ("creativity.retreat.mode", |m| {
            m["creativity"]["retreat"] = json!({ "mode": "chaos" });
        }),
        ("creativity.retreat.template", |m| {
            m["creativity"]["retreat"] = json!({ "template": "x" });
        }),
        ("budget.prices.a/b", |m| {
            m["budget"]["prices"] = json!({ "a/b": { "prompt_per_mtok": -1, "completion_per_mtok": 1, "provider": "p" } });
        }),
        ("budget.prices.openai/gpt-x", |m| {
            m["models"] = json!({ "default": "openai/gpt-x" });
        }),
        ("keys.search", |m| {
            m["keys"] = json!({ "search": "lower_case" });
        }),
        ("keys.openrouter", |m| {
            m["keys"] = json!({ "openrouter": { "provider": "x", "env": "KEY" } });
        }),
        (
            "keys.web_search",
            |m| m["keys"] = json!({ "web_search": { "provider": " ", "env": "KEY" } }),
        ),
        (
            "keys.search",
            |m| m["keys"] = json!({ "search": { "provider": "gemini", "env": "KEY" } }),
        ),
        ("score.scorers", |m| m["score"]["scorers"] = json!([])),
        ("score.scorers.colors", |m| {
            m["score"]["scorers"][0]["judge"] = json!("brief.md");
        }),
        ("score.scorers.colors", |m| {
            m["score"]["scorers"][0]["run"] = json!("  ");
        }),
        ("score.scorers.colors", |m| {
            m["score"]["scorers"][0]["weight"] = json!(0);
        }),
        ("score.scorers.colors", |m| {
            m["score"]["scorers"][0]["raters"] = json!(0);
        }),
        ("score.scorers.colors", |m| {
            let first = m["score"]["scorers"][0].clone();
            m["score"]["scorers"].as_array_mut().unwrap().push(first);
        }),
        ("score.decide.seeds", |m| {
            m["score"]["decide"]["seeds"] = json!([1, 1]);
        }),
        ("score.decide.split", |m| {
            m["score"]["decide"]["test"] = json!("sign");
        }),
        ("score.decide.alpha", |m| {
            m["score"]["decide"]["alpha"] = json!(1.5);
        }),
        ("score.decide.margin", |m| {
            m["score"]["decide"]["margin"] = json!(-1);
        }),
        ("gate[0]", |m| m["gate"] = json!([{ "run": "" }])),
        ("budget.run_usd", |m| m["budget"]["run_usd"] = json!(0)),
        ("budget.total_usd", |m| m["budget"]["total_usd"] = json!(-2)),
        ("budget.score_reserve_usd", |m| {
            m["budget"]["score_reserve_usd"] = json!(-1);
        }),
        ("epochs", |m| m["epochs"] = json!(0)),
        ("env.RSI_SEED", |m| m["env"] = json!({ "RSI_SEED": "1" })),
        ("env.lower", |m| m["env"] = json!({ "lower": "1" })),
    ];
    for (field, edit) in cases {
        assert_eq!(field_error(&with(edit)), field, "case {field}");
    }
}

#[test]
fn a_judge_cannot_decide() {
    let mut files = with(|m| {
        m["score"]["scorers"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "quality", "judge": "rubric.md", "weight": 0.2 }));
        m["score"]["decide"]["on"] = json!("quality");
    });
    files.insert("rubric.md".into(), "Is it clear?".into());
    assert_eq!(field_error(&files), "score.decide.on");
}

#[test]
fn resolves_models_keys_and_rubrics() {
    let mut files = with(|m| {
        m["models"] = json!({ "default": "a/seat", "judge": "a/judge", "retreat": "a/retreat" });
        let price = json!({ "prompt_per_mtok": 0.1, "completion_per_mtok": 0.2, "provider": "a" });
        m["budget"]["prices"] = json!({ "a/seat": price, "a/judge": price, "a/override": price });
        m["creativity"]["retreat"] = json!({ "model": "a/override" });
        m["keys"] = json!({
            "openrouter": "MY_OR_KEY",
            "web_search": { "provider": "gemini", "env": "GEMINI_API_KEY", "seats": ["lead"] }
        });
        m["score"]["scorers"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "quality", "judge": "rubric.md", "weight": 0.2 }));
    });
    files.insert("rubric.md".into(), "Is it clear?".into());
    let parsed = parse(&files).unwrap();
    assert_eq!(parsed.seat_model(), "a/seat");
    assert_eq!(parsed.judge_model(), "a/judge");
    assert_eq!(parsed.retreat_model(), "a/override");
    assert_eq!(parsed.openrouter_env(), "MY_OR_KEY");
    assert_eq!(parsed.required_env(), vec!["GEMINI_API_KEY", "MY_OR_KEY"]);
    let tools: Vec<_> = parsed
        .tool_keys()
        .map(|(name, t)| (name, t.provider.as_str()))
        .collect();
    assert_eq!(tools, vec![("web_search", "gemini")]);
    assert_eq!(parsed.rubrics["quality"], "Is it clear?");
    let used: Vec<String> = parsed.models_used(&BTreeSet::new()).into_iter().collect();
    assert_eq!(used, vec!["a/judge", "a/override", "a/seat"]);
    let retreat = parsed.manifest.creativity.retreat.unwrap();
    assert!(retreat.enabled && retreat.memory);
    assert_eq!(
        (retreat.variations, retreat.turns, retreat.ideas),
        (3, 6, 5)
    );
}

#[test]
fn pins_the_stored_form_with_defaults_filled_in() {
    let parsed = parse(&example()).unwrap();
    let stored = serde_json::to_value(&parsed.manifest).unwrap();
    assert_eq!(
        stored["hive"],
        json!({ "package": "hive", "lead": "lead", "wall_secs": 900, "turn_secs": 1200 })
    );
    assert_eq!(
        stored["score"]["decide"],
        json!({
            "on": "colors", "test": "seeds", "seeds": [1, 2, 3], "split": null, "alpha": 0.1,
            "direction": "minimize", "margin": 0.0, "target": 6.0
        })
    );
    assert_eq!(
        stored["subject"],
        json!({ "kind": "dir", "path": "subject", "branch": null, "editable": ["coloring.py"], "frozen": [], "require_test_change": false })
    );
    let back: Manifest = serde_json::from_value(stored).unwrap();
    assert_eq!(back, parsed.manifest);
}

#[test]
fn env_names_follow_the_shell_convention() {
    assert!(env_name("OPENROUTER_API_KEY") && env_name("_X1"));
    assert!(!env_name("") && !env_name("1X") && !env_name("lower") && !env_name("A-B"));
}

#[test]
fn omitted_fields_take_their_defaults() {
    let files = with(|m| {
        m.as_object_mut().unwrap().remove("epochs");
        m["score"]["decide"]
            .as_object_mut()
            .unwrap()
            .remove("direction");
    });
    let parsed = parse(&files).unwrap();
    assert_eq!(parsed.manifest.epochs, 3);
    assert!((parsed.manifest.score.decide.alpha - 0.10).abs() < 1e-12);
    assert_eq!(
        parsed.manifest.score.decide.direction,
        tinyrsi_score::MetricDirection::Maximize
    );
}
