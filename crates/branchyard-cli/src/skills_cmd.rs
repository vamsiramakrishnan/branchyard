//! `by skills ls|trust|untrust`, and the decision every new local branch
//! takes about its repository's skills. See docs/knowledge-skills.md.
//!
//! `ls` shows the built-in skills and the repository's
//! (`.agents/skills/<name>/SKILL.md`) at HEAD, with whether you trusted
//! them; with a branch, or inside a harness for its own, the set fixed when
//! that branch was created, staged or withheld and why. `trust` records the
//! set digest at HEAD; `untrust` forgets it.
//!
//! Trust is per user, per repository root and per set digest, kept in the
//! trust file `by workspace trust` uses (`trusted-workspaces.json`, or
//! `BRANCHYARD_TRUST_FILE`), in its `skills` map. A new branch stages the
//! repository's skills at its base commit only when their set digest is
//! the one trusted ([`gate`]); a change withholds them until trusted again.
//! A harness on a branch (`BRANCHYARD_BRANCH` set) can never trust them,
//! and a server stages them only when its operator allows them
//! (`allow_repository_skills`), so `--remote` is refused.

use std::path::Path;

use branchyard::skills::{self, Found, RepositorySet, REPOSITORY_SKILLS};
use branchyard::skills::{Layer, SkillEntry};
use branchyard::{BranchSkills, SkillGate, Yard};
use branchyard_workspace::Git;
use serde::Serialize;
use serde_json::{json, Value};

use crate::args::SkillsAction;
use crate::commands::{self, print, Failure, Outcome, Target};
use crate::errors::Report;
use crate::setup_cmd::plural;
use crate::workspace_cmd::{self, in_harness, TrustSection};

/// What a person decided about the repository's skills at HEAD.
enum Trust {
    Trusted,
    Untrusted,
    /// Trusted once, as set `trusted`, and changed since.
    Changed {
        trusted: String,
    },
}

/// The repository's skills at HEAD, and what was decided about them.
enum AtHead {
    /// No skill directory.
    Nothing,
    /// Skills that cannot be used, and why.
    Refused(String),
    Skills {
        set: RepositorySet,
        trust: Trust,
    },
}

/// One skill as `ls` lists it; its description only when read from a
/// commit, as a branch's record keeps none.
#[derive(Serialize)]
struct Listed {
    label: String,
    name: String,
    digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl Listed {
    fn new(entry: &SkillEntry, description: Option<&str>) -> Listed {
        Listed {
            label: entry.label(),
            name: entry.name.clone(),
            digest: entry.digest.clone(),
            description: description.map(str::to_owned),
        }
    }

    fn at_head(set: &RepositorySet) -> Vec<Listed> {
        set.skills
            .iter()
            .map(|skill| Listed::new(&skill.entry(Layer::Repository), Some(&skill.description)))
            .collect()
    }

    fn fixed(skills: &BranchSkills) -> Vec<Listed> {
        skills
            .entries
            .iter()
            .map(|e| Listed::new(e, None))
            .collect()
    }
}

/// The trust file's key for the repository at `root`.
fn key(root: &Path) -> String {
    workspace_cmd::canonical(root).display().to_string()
}

/// The first 12 characters of a digest or commit.
fn short(text: &str) -> &str {
    text.get(..12).unwrap_or(text)
}

fn at_head(root: &Path) -> Result<AtHead, Failure> {
    Ok(match skills::read(root, "HEAD")? {
        Found::Nothing => AtHead::Nothing,
        Found::Refused(why) => AtHead::Refused(why),
        Found::Skills(set) => {
            let trust = match workspace_cmd::trusted_digest(TrustSection::Skills, &key(root))? {
                Some(digest) if digest == set.digest => Trust::Trusted,
                Some(digest) => Trust::Changed { trusted: digest },
                None => Trust::Untrusted,
            };
            AtHead::Skills { set, trust }
        }
    })
}

/// Whether a new branch of the repository at `root` stages its skills:
/// the set digest you trusted, or withheld and why. A harness can never
/// trust them, and a trust file that cannot be read withholds them rather
/// than stopping the run (`by skills ls` reports it).
pub(crate) fn gate(root: &Path) -> SkillGate {
    if in_harness().is_some() {
        return SkillGate::Withheld(
            "a harness running on a branch cannot trust repository skills; a person trusts \
             them with by skills trust, outside any branch"
                .into(),
        );
    }
    match workspace_cmd::trusted_digest(TrustSection::Skills, &key(root)) {
        Ok(Some(digest)) => SkillGate::Trusted(digest),
        Ok(None) => SkillGate::Withheld(
            "you have not trusted this repository's skills; review them with by skills ls, \
             then run by skills trust"
                .into(),
        ),
        Err(failure) => SkillGate::Withheld(format!(
            "your trust decisions could not be read ({failure}); by skills ls says why"
        )),
    }
}

/// `by skills ACTION`.
pub fn main(target: &Target, action: &SkillsAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Report::usage(
            "by skills reads a local repository's skills and your trust decisions; a server \
             stages a repository's skills only when its operator allows them \
             (allow_repository_skills)",
        )
        .next("run by skills without --remote")
        .into());
    }
    match action {
        SkillsAction::Ls { branch } => {
            let yard = commands::open()?;
            match branch.clone().or_else(in_harness) {
                Some(branch) => ls_branch(&yard, &branch, json),
                None => ls_here(&yard, json),
            }
        }
        SkillsAction::Trust => trust(json),
        SkillsAction::Untrust => untrust(json),
    }
}

fn to_json(value: &Value) -> String {
    format!("{value:#}\n")
}

/// Say on standard error when `.agents/skills` has changes not committed:
/// only committed skills count. Whether it has.
fn note_uncommitted(root: &Path) -> Result<bool, Failure> {
    let status = Git::new(root)
        .args(["status", "--porcelain", "--", REPOSITORY_SKILLS])
        .run()
        .map_err(|e| {
            Report::new(
                "git",
                format!("could not read the status of {REPOSITORY_SKILLS}: {e}"),
            )
        })?;
    let uncommitted = !status.trim().is_empty();
    if uncommitted {
        eprintln!("by: {REPOSITORY_SKILLS} has uncommitted changes; only committed skills count");
    }
    Ok(uncommitted)
}

/// The built-in skills' line: how many, and their names.
fn built_in_line(built_in: &[SkillEntry]) -> String {
    let names: Vec<&str> = built_in.iter().map(|e| e.name.as_str()).collect();
    format!(
        "built-in    {} skills: {}\n",
        built_in.len(),
        names.join(", ")
    )
}

/// One indented line per skill: its label, short digest and, when known,
/// its description.
fn skill_lines(skills: &[Listed]) -> String {
    let width = skills.iter().map(|s| s.label.len()).max().unwrap_or(0);
    skills
        .iter()
        .map(|s| {
            let line = format!("  {:<width$}  {}", s.label, short(&s.digest));
            match &s.description {
                Some(text) => format!("{line}  {text}\n"),
                None => format!("{line}\n"),
            }
        })
        .collect()
}

fn ls_here(yard: &Yard, json: bool) -> Outcome {
    let root = workspace_cmd::canonical(yard.root());
    let uncommitted = note_uncommitted(&root)?;
    let built_in = skills::built_in()?;
    let head = at_head(&root)?;
    let trust_file = workspace_cmd::trust_file();
    if json {
        let repository = match &head {
            AtHead::Nothing => json!({ "state": "none", "skills": [] }),
            AtHead::Refused(reason) => {
                json!({ "state": "refused", "reason": reason, "skills": [] })
            }
            AtHead::Skills { set, trust } => {
                let (state, trusted_set) = match trust {
                    Trust::Trusted => ("trusted", None),
                    Trust::Untrusted => ("untrusted", None),
                    Trust::Changed { trusted } => ("changed", Some(trusted)),
                };
                let mut value = json!({
                    "state": state,
                    "commit": set.commit,
                    "set": set.digest,
                    "skills": Listed::at_head(set),
                });
                if let Some(trusted) = trusted_set {
                    value["trusted_set"] = json!(trusted);
                }
                value
            }
        };
        return print(&to_json(&json!({
            "root": root,
            "built_in": built_in,
            "repository": repository,
            "uncommitted": uncommitted,
            "trust_file": trust_file,
        })));
    }
    let mut text = format!("root        {}\n", root.display());
    text.push_str(&built_in_line(&built_in));
    match &head {
        AtHead::Nothing => text.push_str(&format!(
            "repository  none: no {REPOSITORY_SKILLS}/<name>/SKILL.md at HEAD \
             (docs/knowledge-skills.md)\n"
        )),
        AtHead::Refused(reason) => text.push_str(&format!(
            "repository  refused: {reason}; fix it and commit, then by skills trust\n"
        )),
        AtHead::Skills { set, trust } => {
            text.push_str(&format!(
                "repository  {} at commit {} in {REPOSITORY_SKILLS}, set {}\n",
                plural(set.skills.len(), "skill"),
                short(&set.commit),
                short(&set.digest)
            ));
            text.push_str(&skill_lines(&Listed::at_head(set)));
            let state = match trust {
                Trust::Trusted => format!(
                    "trusted ({}): new branches here stage them",
                    trust_file.display()
                ),
                Trust::Untrusted => "not trusted: new branches here withhold them; review them, \
                                     then by skills trust"
                    .to_owned(),
                Trust::Changed { trusted } => format!(
                    "changed since you trusted set {}: new branches here withhold them; review \
                     them, then by skills trust",
                    short(trusted)
                ),
            };
            text.push_str(&format!("trust       {state}\n"));
        }
    }
    print(&text)
}

fn ls_branch(yard: &Yard, branch: &str, json: bool) -> Outcome {
    let fixed = yard.branch_skills(branch)?;
    let built_in = skills::built_in()?;
    if json {
        let repository = match &fixed {
            None => json!({ "state": "none", "skills": [] }),
            Some(fixed) => {
                let mut value = json!({
                    "state": if fixed.withheld.is_some() { "withheld" } else { "staged" },
                    "commit": fixed.commit,
                    "set": fixed.digest,
                    "skills": Listed::fixed(fixed),
                });
                if let Some(reason) = &fixed.withheld {
                    value["reason"] = json!(reason);
                }
                value
            }
        };
        return print(&to_json(&json!({
            "branch": branch,
            "built_in": built_in,
            "repository": repository,
        })));
    }
    let mut text = format!("branch      {branch}\n");
    text.push_str(&built_in_line(&built_in));
    match &fixed {
        None => text.push_str(&format!(
            "repository  none: the commit it started from has no {REPOSITORY_SKILLS}\n"
        )),
        Some(fixed) => text.push_str(&branch_set(fixed)),
    }
    print(&text)
}

/// The text of a branch's repository skills.
fn branch_set(fixed: &BranchSkills) -> String {
    let mut text = format!(
        "repository  {} from commit {}, set {}, fixed when the branch was created\n",
        plural(fixed.entries.len(), "skill"),
        short(&fixed.commit),
        short(&fixed.digest)
    );
    text.push_str(&skill_lines(&Listed::fixed(fixed)));
    match &fixed.withheld {
        None => text.push_str("state       staged\n"),
        Some(reason) => text.push_str(&format!("state       withheld: {reason}\n")),
    }
    text
}

fn trust(json: bool) -> Outcome {
    if in_harness().is_some() {
        return Err(Report::new(
            "denied",
            "a harness running on a branch cannot trust repository skills; trusting is a \
             person's decision, made outside any branch",
        )
        .next("run by skills trust in your own terminal")
        .into());
    }
    let root = workspace_cmd::canonical(commands::open()?.root());
    note_uncommitted(&root)?;
    let set = match at_head(&root)? {
        AtHead::Nothing => {
            return Err(Report::new(
                "config",
                format!("{} has no skills to trust at HEAD", root.display()),
            )
            .next(format!(
                "add {REPOSITORY_SKILLS}/<name>/SKILL.md and commit it (docs/knowledge-skills.md)"
            ))
            .into())
        }
        AtHead::Refused(reason) => {
            return Err(Report::new("config", reason)
                .next("fix it and commit, then by skills trust")
                .into())
        }
        AtHead::Skills { set, .. } => set,
    };
    workspace_cmd::record_trust(TrustSection::Skills, &key(&root), &set.digest)?;
    let labels: Vec<String> = set.entries().iter().map(SkillEntry::label).collect();
    if json {
        return print(&to_json(&json!({
            "root": root,
            "commit": set.commit,
            "set": set.digest,
            "skills": labels,
            "trusted": true,
            "trust_file": workspace_cmd::trust_file(),
        })));
    }
    print(&format!(
        "trusted {} at commit {} (set {}): {}\nnew branches of {} stage them; after a change, run \
         by skills trust again\n",
        plural(labels.len(), "repository skill"),
        short(&set.commit),
        short(&set.digest),
        labels.join(", "),
        root.display()
    ))
}

fn untrust(json: bool) -> Outcome {
    let root = workspace_cmd::canonical(commands::open()?.root());
    let removed = workspace_cmd::forget_trust(TrustSection::Skills, &key(&root))?;
    if json {
        return print(&to_json(&json!({ "root": root, "removed": removed })));
    }
    match removed {
        true => print(&format!(
            "{}'s repository skills are no longer trusted; new branches withhold them\n",
            root.display()
        )),
        false => print(&format!(
            "{}'s repository skills were not trusted\n",
            root.display()
        )),
    }
}
