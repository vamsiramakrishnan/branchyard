//! Branchyard's setup: one declarative interview per topic, with two
//! front-ends over it. `by init` asks a person in a terminal wizard; a
//! coding harness drives the same questions through the JSON protocol
//! (`by init TOPIC --json --next`), asking the person with its own question
//! tool, then applies the plan after they agree. See `docs/setup.md`.
//!
//! This crate does no I/O. The machine and repository come in through a
//! [`Probe`], randomness through [`Entropy`], and each generated file is
//! checked by a [`Validator`], which `by` backs with the real loaders (the
//! server's configuration loader, `by rig check`'s planner, this crate's
//! own [`config::parse`]).
//!
//! ```
//! use branchyard_setup::{next, probe::{FakeProbe, CountingEntropy}, BuiltinValidator, Topic};
//! let probe = FakeProbe::typical();
//! let first = next(Topic::Project, &probe, &Default::default(), false, &mut CountingEntropy(0), &BuiltinValidator);
//! assert!(!first.done && first.questions.len() <= 4);
//! let done = next(Topic::Project, &probe, &Default::default(), true, &mut CountingEntropy(0), &BuiltinValidator);
//! assert!(done.done && done.plan.unwrap().valid);
//! ```

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod config;
pub mod interview;
pub mod plan;
pub mod probe;
pub mod protocol;
pub mod schema;
pub mod skills;
mod topics;

pub use interview::{Answers, Choice, Condition, Kind, Question, Rule};
pub use plan::{
    ArtifactKind, BuiltinValidator, FileAction, FollowUp, Plan, PlannedFile, Validation, Validator,
};
pub use probe::{Entropy, Facts, Probe};
pub use protocol::Response;

/// The protocol's name and version, in every JSON response.
pub const PROTOCOL: &str = "branchyard.setup/v1";

/// What an interview sets up.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// `branchyard.toml`: defaults for local runs.
    Project,
    /// A rig spec: a team of seats.
    Rig,
    /// A server configuration, credentials and token files.
    Server,
    /// A Docker Compose deployment of the server.
    Deploy,
    /// The Branchyard skills for Claude Code or Codex.
    Plugin,
}

impl Topic {
    pub const ALL: [Topic; 5] = [
        Topic::Project,
        Topic::Rig,
        Topic::Server,
        Topic::Deploy,
        Topic::Plugin,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Topic::Project => "project",
            Topic::Rig => "rig",
            Topic::Server => "server",
            Topic::Deploy => "deploy",
            Topic::Plugin => "plugin",
        }
    }

    pub fn parse(text: &str) -> Option<Topic> {
        Topic::ALL.into_iter().find(|t| t.id() == text)
    }

    pub fn title(self) -> &'static str {
        match self {
            Topic::Project => "Project defaults",
            Topic::Rig => "A rig: a team of harnesses",
            Topic::Server => "A Branchyard server",
            Topic::Deploy => "A Docker Compose deployment",
            Topic::Plugin => "Skills for your coding harness",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Topic::Project => "branchyard.toml: default harness, model, limits, permissions, isolation, check, secrets by name, and a server to use",
            Topic::Rig => "A rig TOML: a lead seat and the seats it delegates to, each with its harness, budget and policy",
            Topic::Server => "A server configuration: listen address and TLS, SQLite or PostgreSQL, tenants with hashed credentials and 0600 token files, quotas, providers, secrets, webhooks",
            Topic::Deploy => "compose.yaml with PostgreSQL, a server configuration and generated secret files",
            Topic::Plugin => "Install the setup and delegate skills into Claude Code or Codex",
        }
    }
}

/// Every question `topic` may ask, given the facts and the answers so far.
pub fn questions(topic: Topic, facts: &Facts, answers: &Answers) -> Vec<Question> {
    match topic {
        Topic::Project => topics::project::questions(facts, answers),
        Topic::Rig => topics::rig::questions(facts, answers),
        Topic::Server => topics::server::questions(facts, answers),
        Topic::Deploy => topics::deploy::questions(facts, answers),
        Topic::Plugin => topics::plugin::questions(facts, answers),
    }
}

/// The plan for complete `answers`, each file validated.
pub fn plan(
    topic: Topic,
    facts: &Facts,
    answers: &Answers,
    probe: &dyn Probe,
    entropy: &mut dyn Entropy,
    validator: &dyn Validator,
) -> Plan {
    let mut plan = match topic {
        Topic::Project => topics::project::plan(facts, answers, probe),
        Topic::Rig => topics::rig::plan(facts, answers, probe),
        Topic::Server => topics::server::plan(facts, answers, probe, entropy),
        Topic::Deploy => topics::deploy::plan(facts, answers, probe, entropy),
        Topic::Plugin => topics::plugin::plan(facts, answers, probe),
    };
    plan.validate(validator);
    plan
}

/// One protocol step: the next questions for `raw` answers, or, when none
/// remain, the validated plan. With `defaults`, unanswered questions take
/// their defaults.
pub fn next(
    topic: Topic,
    probe: &dyn Probe,
    raw: &BTreeMap<String, Value>,
    defaults: bool,
    entropy: &mut dyn Entropy,
    validator: &dyn Validator,
) -> Response {
    let facts = Facts::gather(probe);
    let state = interview::resolve(&|answers| questions(topic, &facts, answers), raw, defaults);
    let plan = state
        .done()
        .then(|| plan(topic, &facts, &state.answers, probe, entropy, validator));
    Response::new(topic, &facts, state, plan)
}

/// The SHA-256 of `bytes`, lowercase hex, as the server hashes tokens.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}
