# RSI CLI: file-defined loops

**Status:** Accepted. **Owner:** TinyRSI maintainers.

Related: [loop trace](loop-trace.md), [script loop](script-loop.md),
[rsi-viz](rsi-viz.md), [plan](../plans/2026-10-11-rsi-cli.md).

## Problem

Running an improvement loop with TinyRSI today means writing Rust against the
library: a subject literal, a supervisor, a scorer, a policy, a trace sink. Real
users do not. The OpenHuman memory benchmark loop (`oh-bench-rsi`) reimplemented
the loop in Python, the agent swarm in a standalone Rust crate, a spend-capped
model gateway in another, and copied the trace schema by hand with placeholder
values. The generic parts of an RSI loop — epochs, candidate workspaces,
agents, scoring, decision, promotion, spend, trace, run history — belong in
TinyRSI; only the benchmark belongs to the user.

## Goals and non-goals

- Goal: a `tinyrsi` binary (crate `tinyrsi-cli`) that runs a loop defined as a
  folder of JSON and Markdown plus small step scripts in any language.
- Goal: the agents that write candidates are always a tinyhivemind hive,
  declared in the hive language inside the loop folder.
- Goal: user-defined score functions (commands and Markdown-rubric judges),
  creativity settings (temperature, tinybackrooms retreat) and named access to
  keys (OpenRouter, search) without ever putting a secret in a file.
- Goal: every run is stored durably, and the binary serves the visualizer over
  the store, by default alongside `run`.
- Non-goal: replacing the existing library crates. The CLI reuses them.
- Non-goal: sandboxing step commands. They run with the user's privileges.

## The loop folder

```text
<loop>/
  rsi.json          manifest (required)
  brief.md          goal prose for the hive (required)
  hive/             tinyhivemind package (required)
  scorers/ gates/   step scripts and judge rubrics referenced by rsi.json
```

Every path in `rsi.json` is relative to the loop folder, plain (no `..`, no
absolute paths) and must exist. The parser, like the hive language, takes an
in-memory map of relative path to text and opens no files.

## `rsi.json`

```json
{
  "$schema": "https://tinyhumans.ai/schemas/rsi/v1",
  "version": 1,
  "name": "graph-coloring",
  "subject": { "kind": "dir", "path": "subject", "editable": ["coloring.py"] },
  "brief": "brief.md",
  "hive": { "package": "hive", "lead": "lead", "wall_secs": 900 },
  "models": { "default": "z-ai/glm-5.3-flash" },
  "score": {
    "scorers": [ { "id": "colors", "run": "python3 scorers/check.py" } ],
    "decide": { "on": "colors", "test": "seeds", "seeds": [1, 2, 3] }
  },
  "epochs": 3
}
```

Unknown fields fail. `$schema` and `version` must match exactly.

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Loop name, `[a-z0-9-]`, names the run-store folder. |
| `subject` | yes | What is improved; see below. |
| `brief` | yes | Markdown prompt template. |
| `hive` | yes | `package` (folder), `lead` (seat that receives the brief), `wall_secs` (default 3600), `turn_secs` (default 1200). |
| `models` | no | `default`, `judge`, `retreat` model ids (OpenRouter names). |
| `creativity` | no | `temperature` and `retreat`; see below. |
| `keys` | no | Capability to env-var name; see below. |
| `score` | yes | `scorers` (non-empty) and `decide`. |
| `gate` | no | Ordered list of `{ "run", "cwd"?, "timeout_secs"? }`. |
| `budget` | no | `run_usd`, `total_usd`, `score_reserve_usd`, `prices`. |
| `epochs` | no | Maximum epochs, default 3. |
| `patience` | no | Epochs without promotion before stopping, default 2. |
| `env` | no | Extra environment for every step. |

**`subject`.** `kind` is `dir` or `git`. `path` is the subject directory,
relative to the loop folder. A `dir` path must stay inside the loop folder; a
`git` path may be absolute or leave it with `..`, since the repository usually
lives elsewhere. `editable` lists path prefixes the hive may change;
`frozen` lists prefixes it must not, and wins over `editable`. A `git` subject
also names the `branch` that holds the incumbent; promotion fast-forwards it.
`require_test_change` (default `false`) rejects candidates that touch no file
whose path contains `test`.

**`models`.** Every model call goes through the run's spend gateway, which only
allows models with a price ceiling. `z-ai/glm-5.3-flash` (the default for every
role) and `openai/text-embedding-3-small` are built in; any other model a seat,
judge or the retreat uses needs an entry in `budget.prices`:
`{ "<model>": { "prompt_per_mtok", "completion_per_mtok", "provider" } }`,
dollars per million tokens and the one upstream provider it may route to.
`check` fails on a used model with no price.

**`creativity`.** `temperature` (0–2) is the default for seats that do not set
`config.temperature` in `hive.json`. `retreat` runs a tinybackrooms creative
retreat before the hive each epoch: `enabled`, `variations` (independent
conversations, default 3), `turns` (per conversation, default 6), `ideas` (how
many reach the brief, default 5), `mode` (`backrooms`, `brainstorm` or
`critic_pair`), `template` (`cli`, `fugue` or `student`), `model`,
`temperature`, and `memory` (default on: earlier ideas and whether their epoch
promoted a candidate are fed back as prior ideas to move away from).

**`keys`.** A map from capability to the *name* of an environment variable.
Values never appear in files, the run store, traces or prompts.
`openrouter` (default `OPENROUTER_API_KEY`) is read by the gateway only.
Tool capabilities such as `search` take `{ "provider", "env", "seats"? }` and
enable that tool for the listed seats (all seats when omitted). Values are read
from the process environment, then from a `.env` file in the loop folder.

**`score.scorers`.** Each has a unique `id`, a `weight` (default 1) and exactly
one of:

- `run`: a command (split on whitespace; the program is resolved on `PATH`)
  run in the loop folder under the step contract. It writes
  `$RSI_OUT/result.json`, or prints a `METRIC <name>=<float>` line whose value
  becomes `score`.
- `judge`: a Markdown rubric graded by `LlmJudge` against the candidate diff.
  `raters` (default 3). A judge is never eligible to accept on its own.

**`score.decide`.** `on` names the deciding `run` scorer. `test` is:

- `seeds`: every `run` scorer runs once per seed in `seeds` (at least two
  distinct) and the paired `tinyrsi_score::decide` decides on the deciding
  scorer's per-seed scores, with `direction` (`maximize` default, or
  `minimize`), `margin` (in quality units, 0–1) and `target`. Judge grades are
  mixed into the per-seed qualities by weight; the incumbent counts as a
  neutral 0.5. Only the metric is accept-eligible, so a judge can temper or
  block an acceptance but never cause one;
- `sign`: the scorer runs once and reports per-item 0/1 scores for `split`; an
  exact one-sided sign test on discordant pairs at `alpha` (default 0.10)
  decides. Judges are recorded but do not vote.

Other `run` scorers are recorded and shown in the brief, never decided on.

## Step contract

Gate and scorer commands run with a timeout (default 600 s), an output cap, and
process-group kill. Environment: `PATH`, `HOME`, `TMPDIR`, the `env` map, and

| Variable | Meaning |
| --- | --- |
| `RSI_CANDIDATE` | Candidate checkout being judged. |
| `RSI_INCUMBENT` | Incumbent checkout. |
| `RSI_OUT` | Empty directory for this step's outputs. |
| `RSI_RUN_DIR` | The run's folder in the store. |
| `RSI_LOOP_DIR` | The loop folder. |
| `RSI_EPOCH` | Epoch number (0 is the baseline). |
| `RSI_LABEL` | `incumbent` or the candidate id. |
| `RSI_SEED` | Seed for this invocation. |
| `RSI_MODEL_URL` | OpenAI-compatible base URL through the gateway, tagged with the step id. |
| `RSI_GATEWAY` | The gateway's untagged base; a step tags its own calls as `$RSI_GATEWAY/__tag/<phase>/<seat>/v1`. |

`result.json`:

```json
{ "score": 0.62,
  "splits": { "test": { "items": { "q17": 1, "q18": 0 } } },
  "failures": [ { "id": "q3", "type": "temporal", "text": "shown to the hive" } ],
  "metrics": { "pack_tokens_mean": 812 } }
```

Only `score` (finite) is required. `failures` feed the brief. A gate passes on
exit 0; the last non-empty stderr line is recorded as the reason.

## One run

1. Validate the folder (`check`), snapshot it into the run store, start the
   gateway and (by default) the UI server.
2. Baseline: score the incumbent, reusing a cached result for the same loop,
   subject revision and scorer definition.
3. Each epoch: render the brief (`{{incumbent}}`, `{{history}}`,
   `{{failures}}`, `{{ideas}}`; unused sections are appended), optionally run
   the retreat, give each editing seat its own candidate checkout, run the hive
   until idle or `wall_secs`, then for each changed candidate: surface check,
   gates, scorers if the `score_reserve_usd` is still affordable, decision.
4. Promote the accepted candidate with the best deciding score.
5. Stop on `goal_reached`, `stalled`, `budget`, `max_epochs` or `error`.

Candidates with no change are recorded as `empty` and not scored.

## Run store

`$TINYRSI_HOME` (default `~/.tinyrsi`, overridable with `--home`):

```text
runs/<loop>/<run-id>/
  loop/          frozen copy of the loop folder
  run.json       status, times, stop reason, incumbent, spend
  events.jsonl   loop trace (loop-trace.md)
  ledger.json    every candidate with gate, scores and decision
  epoch-<n>/     brief.md, ideas.json, swarm/, candidates/, steps/
cache/           incumbent score cache
spend.json       total spend ledger across runs
retreat/         retreat memory per loop
```

`run.json` and `ledger.json` are rewritten atomically. A run whose process died
is shown as `interrupted`.

## CLI

```text
tinyrsi check <loop>
tinyrsi run   <loop> [--run ID] [--epochs N] [--port 8787] [--no-serve]
tinyrsi score <loop> --label L [--checkout P]
tinyrsi runs  [--loop NAME]
tinyrsi show  <loop>/<run-id>
tinyrsi serve [--port 8787]
```

`run` serves the UI on `127.0.0.1:<port>` by default and prints its URL.
The UI is one page embedded in the binary — no Node toolchain is involved —
over a read-only API: `/api/runs`, `/api/runs/<loop>/<run>/events` (SSE,
replay then tail) and `/api/runs/<loop>/<run>/files/<path>` for `run.json`,
`ledger.json`, an epoch's `brief.md` and a candidate's `change.diff`. The store
layout is also what `apps/rsi-viz` discovers, with `RSI_RUN_ROOT=$TINYRSI_HOME`.

Exit status is 0 on success, 1 on a loop error, 2 on invalid input.

## Invariants

- No secret value is written anywhere under the store or the loop folder.
- The incumbent only changes by promotion of an accepted candidate.
- A candidate's scores are only compared against the incumbent's scores on the
  same seeds or items.
- An uncalibrated judge alone cannot accept a candidate.

## Acceptance criteria

- `tinyrsi check` rejects every malformed field with a specific error.
- An offline example loop runs end to end through the binary, reaches its goal,
  appears in `tinyrsi runs`, and is visible through `tinyrsi serve`.
- oh-bench-rsi's loop is expressed as a loop folder with no Rust of its own.

## Open questions

None blocking. Which search providers OpenHuman's tool layer exposes is checked
when the swarm lands; a missing provider is added upstream.
