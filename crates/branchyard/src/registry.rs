//! The local skill registry (`docs/skill-registry.md`, R2): every skill a
//! turn started here may see, with its source, digest and state; the tool
//! tree such a turn would mount, written without running one; the same
//! ranker a turn's `bin/tools` runs, over that tree; and what turns did with
//! their skills, from the record. `by skills list | search | show | index |
//! usage` are this module's commands. A server's registry (R4) is not built.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::skills::{ForTurn, RepositorySet, Skill, Source, UserSet};
use crate::{Activity, Error, SkillActivity, Yard};

/// A skill's state here, two of the tools model's five
/// (`docs/tools-model.md`, section 1): a local turn has no administrator
/// and no workspace defaults.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    /// A turn started here mounts it: a built-in skill, a trusted
    /// repository set's skill, the person's own.
    Ready,
    /// It is here and a turn does not mount it: a repository set nobody
    /// trusted (or one changed since), or a skill in the person's folder
    /// that is withheld by name. `reason` says why.
    Available,
}

impl SkillState {
    /// The state as serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            SkillState::Ready => "ready",
            SkillState::Available => "available",
        }
    }
}

/// What the registry knows about one skill here.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRecord {
    /// How the tree names it: `pptx`, `repository:style`, `user:memory-parser`.
    pub label: String,
    /// Its directory's name.
    pub name: String,
    /// Where it comes from.
    pub source: Source,
    /// BLAKE3 over its files; empty for a skill withheld before it was read.
    pub digest: String,
    /// Its frontmatter's description.
    pub description: String,
    /// Whether a turn started here mounts it.
    pub state: SkillState,
    /// Why it is not ready, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Files in the bundle.
    pub files: u32,
    /// Bytes in the bundle.
    pub size: u64,
    /// Its `references/*.md`, the level-3 documents, by path in the bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
}

/// The repository's skills at the commit a new branch here would start
/// from, and why a turn would not mount them when it would not.
#[derive(Clone, Copy, Debug)]
pub struct RepositorySkills<'a> {
    /// The set read from the commit.
    pub set: &'a RepositorySet,
    /// `None` when the set is trusted; else why it is withheld.
    pub withheld: Option<&'a str>,
}

/// The person's own skills as a turn here would read them: the folder
/// `dir`, with a skill that has a built-in's name, or one the trusted
/// repository set names, withheld as a turn withholds them (`skills::user_for_turn`).
pub fn user_skills(dir: &Path, repository: Option<RepositorySkills<'_>>) -> UserSet {
    let staged = match repository {
        Some(RepositorySkills {
            set,
            withheld: None,
        }) => ForTurn::Staged(set.clone()),
        _ => ForTurn::Nothing,
    };
    crate::skills::user_for_turn(dir, &staged)
}

/// Every skill here: the built-in skills, the repository's and the
/// person's, each with its state. Built-in first, in catalogue order, then
/// the repository's, then the person's, each in name order.
pub fn records(
    repository: Option<RepositorySkills<'_>>,
    user: Option<&UserSet>,
) -> Result<Vec<SkillRecord>, Error> {
    let mut out = Vec::new();
    for skill in crate::capabilities::catalogued().map_err(Error::State)? {
        let files: Vec<(&str, &str)> = skill.files.iter().map(|(p, c)| (*p, *c)).collect();
        out.push(SkillRecord {
            label: skill.name.to_owned(),
            name: skill.name.to_owned(),
            source: Source::BuiltIn,
            digest: crate::toolstore::skill_digest(skill),
            description: crate::capabilities::description_of(skill).map_err(Error::State)?,
            state: SkillState::Ready,
            reason: None,
            files: u32::try_from(files.len()).unwrap_or(u32::MAX),
            size: files.iter().map(|(_, c)| c.len() as u64).sum(),
            references: references_of(files.iter().map(|(p, _)| *p)),
        });
    }
    if let Some(RepositorySkills { set, withheld }) = repository {
        for skill in &set.skills {
            out.push(record_of(skill, Source::Repository, withheld));
        }
    }
    if let Some(user) = user {
        for skill in &user.skills {
            out.push(record_of(skill, Source::User, None));
        }
        for withheld in &user.withheld {
            out.push(SkillRecord {
                label: branchyard_provision::skillset::label(Source::User, &withheld.name),
                name: withheld.name.clone(),
                source: Source::User,
                digest: String::new(),
                description: String::new(),
                state: SkillState::Available,
                reason: Some(withheld.reason.clone()),
                files: 0,
                size: 0,
                references: Vec::new(),
            });
        }
    }
    Ok(out)
}

fn record_of(skill: &Skill, source: Source, withheld: Option<&str>) -> SkillRecord {
    SkillRecord {
        label: branchyard_provision::skillset::label(source, &skill.name),
        name: skill.name.clone(),
        source,
        digest: skill.digest.clone(),
        description: skill.description.clone(),
        state: match withheld {
            None => SkillState::Ready,
            Some(_) => SkillState::Available,
        },
        reason: withheld.map(str::to_owned),
        files: u32::try_from(skill.files.len()).unwrap_or(u32::MAX),
        size: skill.files.iter().map(|(_, c)| c.len() as u64).sum(),
        references: references_of(skill.files.iter().map(|(p, _)| p.as_str())),
    }
}

/// The `references/*.md` among `paths`, in path order.
fn references_of<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    paths
        .filter(|p| {
            p.starts_with("references/") && p.ends_with(".md") && p.matches('/').count() == 1
        })
        .map(str::to_owned)
        .collect()
}

/// The tool tree [`write_index`] wrote.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// The tree: `OUT/.branchyard/tools`, with `INDEX.md` and `bin/tools`.
    pub tree: PathBuf,
    /// The skills its `INDEX.md` names.
    pub chosen: Vec<String>,
    /// The skills it counts in its overflow line: staged and findable.
    pub mounted: Vec<String>,
}

/// Write the tool tree a turn started here would mount where linking is
/// free, under `out` as its home (`out/.branchyard/capabilities`,
/// `out/.branchyard/tools`), without running a turn: every built-in skill,
/// the repository's trusted set and the person's own, with `chosen` (the
/// built-in skills a `--skill` would pick; `None` picks every built-in
/// skill) named in `INDEX.md` and the rest counted. For `python3
/// tools/skill_eval.py rank --tree` and `sizes --tree`, and for
/// [`search`]. No connectors, no helpers.
pub fn write_index(
    out: &Path,
    chosen: Option<&[String]>,
    repository: Option<&RepositorySet>,
    user: Option<&UserSet>,
) -> Result<Index, Error> {
    std::fs::create_dir_all(out)?;
    let staged = crate::capabilities::stage_from(
        out,
        repository,
        user,
        crate::capabilities::Choice {
            chosen,
            allowed: None,
            mount_ready: true,
        },
        None,
    )
    .map_err(Error::State)?;
    crate::tooltree::mount(out, out, &staged.chosen, &staged.mounted, &[], None, None)
        .map_err(Error::State)?;
    Ok(Index {
        tree: out.join(crate::tooltree::DIRECTORY),
        chosen: staged.chosen.iter().map(|t| t.id.clone()).collect(),
        mounted: staged.mounted.iter().map(|t| t.id.clone()).collect(),
    })
}

/// One result of [`search`]: a line of `tools search`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// `skill`, `reference`, `connector`, `operation` or `helper`.
    pub kind: String,
    /// The id as the tree names it.
    pub id: String,
    /// Its one-line description, as the script prints it.
    pub description: String,
}

/// Run the tree's own `bin/tools search` (the `find-tools` skill's script,
/// the one ranker) over `tree` with `words`, up to `limit` hits; `plain`
/// is BM25 alone. Nothing found is an empty list; the script refusing to
/// run is an error naming what it said.
pub fn search(tree: &Path, words: &[String], limit: usize, plain: bool) -> Result<Vec<Hit>, Error> {
    let mut command = branchyard_support::child::command("python3");
    command
        .arg(tree.join("bin/tools"))
        .arg("search")
        .arg("--limit")
        .arg(limit.max(1).to_string());
    if plain {
        command.arg("--plain");
    }
    command.args(words);
    let output = command
        .output()
        .map_err(|e| Error::State(format!("could not run {}/bin/tools: {e}", tree.display())))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match output.status.code() {
        Some(0) => {}
        // Nothing matched: the script says so on standard error.
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(Error::State(format!(
                "bin/tools search failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
    Ok(stdout.lines().filter_map(parse_hit).collect())
}

/// `kind       id: description` as `tools search` prints it.
fn parse_hit(line: &str) -> Option<Hit> {
    let (kind, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (id, description) = match rest.split_once(": ") {
        Some((id, description)) => (id, description),
        None => (rest, ""),
    };
    Some(Hit {
        kind: kind.to_owned(),
        id: id.to_owned(),
        description: description.to_owned(),
    })
}

/// What turns did with one skill, reference, connector or helper.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUsage {
    /// The id as the tree names it.
    pub id: String,
    /// Searches that ranked it among their results.
    pub found: u64,
    /// Times it was shown (`tools show`).
    pub opened: u64,
    /// Times one of its scripts ran.
    pub invoked: u64,
    /// When it was last opened or invoked, milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<u64>,
}

/// What turns did with their skills, from the record's `skill` events.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Events before this were left out; `None` counts them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// Searches run.
    pub searches: u64,
    /// Searches followed by no open before the branch's next turn (or the
    /// end of the record).
    pub searches_unopened: u64,
    /// Documents shown.
    pub opens: u64,
    /// Scripts run.
    pub invocations: u64,
    /// By id, most opened first, then most found.
    pub skills: Vec<SkillUsage>,
}

/// The most events one page of the feed reads.
const PAGE: usize = 5_000;

/// Searches, opens and invocations across every branch's record, from
/// `since_ms` on when given. A local measure: the events come from a
/// guest-written log, so a turn can inflate them (`docs/skill-registry.md`,
/// section 7).
pub fn usage(yard: &Yard, since_ms: Option<u64>) -> Result<Usage, Error> {
    let mut out = Usage {
        since_ms,
        ..Usage::default()
    };
    let mut by_id: BTreeMap<String, SkillUsage> = BTreeMap::new();
    let mut pending: BTreeMap<String, u64> = BTreeMap::new();
    let mut cursor = 0;
    loop {
        let page = yard.events_since(cursor, PAGE)?;
        if page.events.is_empty() {
            break;
        }
        for feed in &page.events {
            let at = feed.event.at_ms;
            match &feed.event.activity {
                Activity::Prompt(_) => {
                    out.searches_unopened += pending.remove(&feed.branch).unwrap_or(0);
                }
                Activity::Skill(activity) if since_ms.is_none_or(|since| at >= since) => {
                    match activity.as_ref() {
                        SkillActivity::Searched { results, .. } => {
                            out.searches += 1;
                            *pending.entry(feed.branch.clone()).or_default() += 1;
                            for result in results {
                                let id =
                                    result.split_once(':').map_or(result.as_str(), |(_, id)| id);
                                entry(&mut by_id, id).found += 1;
                            }
                        }
                        SkillActivity::Opened { id, .. } => {
                            out.opens += 1;
                            pending.remove(&feed.branch);
                            let row = entry(&mut by_id, id);
                            row.opened += 1;
                            row.last_ms = Some(row.last_ms.map_or(at, |last| last.max(at)));
                        }
                        SkillActivity::Invoked { id, .. } => {
                            out.invocations += 1;
                            let row = entry(&mut by_id, id);
                            row.invoked += 1;
                            row.last_ms = Some(row.last_ms.map_or(at, |last| last.max(at)));
                        }
                    }
                }
                _ => {}
            }
        }
        cursor = page.next_cursor;
    }
    out.searches_unopened += pending.values().sum::<u64>();
    let mut skills: Vec<SkillUsage> = by_id.into_values().collect();
    skills.sort_by(|a, b| {
        (b.opened, b.found, b.invoked, &a.id).cmp(&(a.opened, a.found, a.invoked, &b.id))
    });
    out.skills = skills;
    Ok(out)
}

fn entry<'a>(by_id: &'a mut BTreeMap<String, SkillUsage>, id: &str) -> &'a mut SkillUsage {
    by_id.entry(id.to_owned()).or_insert_with(|| SkillUsage {
        id: id.to_owned(),
        ..SkillUsage::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_provision::skillset;

    fn skill(name: &str, body: &str, extra: &[(&str, &str)]) -> Skill {
        let mut files = vec![(
            "SKILL.md".to_owned(),
            format!("---\nname: {name}\ndescription: The {name} skill.\n---\n{body}\n"),
        )];
        files.extend(
            extra
                .iter()
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned())),
        );
        Skill::new(name, files).unwrap()
    }

    fn repository(skills: Vec<Skill>) -> RepositorySet {
        let entries: Vec<_> = skills.iter().map(|s| s.entry(Source::Repository)).collect();
        RepositorySet {
            commit: "0123456789abcdef".into(),
            digest: skillset::set_digest(&entries),
            skills,
        }
    }

    #[test]
    fn records_list_every_source_with_its_state_and_references() {
        let set = repository(vec![skill(
            "style",
            "Plain.",
            &[
                ("references/tone.md", "# Tone\n"),
                ("references/deep/no.md", "# Not a level-3 document\n"),
                ("scripts/x.py", "print(1)\n"),
            ],
        )]);
        let mine = UserSet {
            skills: vec![skill("memory-parser", "Mine.", &[])],
            withheld: vec![crate::skills::WithheldSkill {
                name: "docx".into(),
                reason: "docx is a built-in skill's name, which is reserved".into(),
            }],
        };
        let all = records(
            Some(RepositorySkills {
                set: &set,
                withheld: None,
            }),
            Some(&mine),
        )
        .unwrap();
        let built_in = all.iter().filter(|r| r.source == Source::BuiltIn).count();
        assert_eq!(built_in, crate::capabilities::catalogued().unwrap().len());
        let pptx = all.iter().find(|r| r.label == "pptx").unwrap();
        assert_eq!(pptx.state, SkillState::Ready);
        assert!(pptx.size > 0 && pptx.files > 1);
        assert_eq!(pptx.digest.len(), 64);
        let calculate = all.iter().find(|r| r.label == "calculate").unwrap();
        assert!(calculate
            .references
            .iter()
            .all(|r| r.starts_with("references/")));
        assert!(
            !calculate.references.is_empty(),
            "calculate ships references"
        );
        let style = all.iter().find(|r| r.label == "repository:style").unwrap();
        assert_eq!(style.state, SkillState::Ready);
        assert_eq!(style.references, ["references/tone.md"]);
        assert_eq!((style.files, style.name.as_str()), (4, "style"));
        assert_eq!(style.description, "The style skill.");
        let mine_record = all
            .iter()
            .find(|r| r.label == "user:memory-parser")
            .unwrap();
        assert_eq!(mine_record.state, SkillState::Ready);
        let withheld = all.iter().find(|r| r.label == "user:docx").unwrap();
        assert_eq!(withheld.state, SkillState::Available);
        assert!(withheld.reason.as_deref().unwrap().contains("reserved"));
        assert!(withheld.digest.is_empty());
        // An untrusted set is available, not ready, with the reason on each skill.
        let untrusted = records(
            Some(RepositorySkills {
                set: &set,
                withheld: Some("you have not trusted this repository's skills"),
            }),
            None,
        )
        .unwrap();
        let style = untrusted
            .iter()
            .find(|r| r.label == "repository:style")
            .unwrap();
        assert_eq!(style.state, SkillState::Available);
        assert!(style.reason.as_deref().unwrap().contains("trusted"));
        assert_eq!(SkillState::Ready.as_str(), "ready");
    }

    #[test]
    fn user_skills_follow_the_turns_precedence() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["memory-parser", "style", "docx"] {
            let path = dir.path().join(name).join("SKILL.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("---\nname: {name}\ndescription: d\n---\n")).unwrap();
        }
        let set = repository(vec![skill("style", "theirs", &[])]);
        let trusted = user_skills(
            dir.path(),
            Some(RepositorySkills {
                set: &set,
                withheld: None,
            }),
        );
        let names: Vec<&str> = trusted.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["memory-parser"]);
        let withheld: Vec<&str> = trusted.withheld.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(withheld, ["docx", "style"]);
        // An untrusted repository set shadows nothing.
        let untrusted = user_skills(
            dir.path(),
            Some(RepositorySkills {
                set: &set,
                withheld: Some("not trusted"),
            }),
        );
        let names: Vec<&str> = untrusted.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["memory-parser", "style"]);
    }

    /// The tree `rank --tree` measures: every skill in it, the chosen ones
    /// named, the rest counted, and the script finding a mounted one.
    #[test]
    fn the_index_is_a_tree_the_script_searches() {
        let out = tempfile::tempdir().unwrap();
        let set = repository(vec![skill(
            "style",
            "House style for release notes.",
            &[(
                "references/tone.md",
                "# Tone\n\nKeep release notes dry and short.\n",
            )],
        )]);
        let chosen = ["find-tools".to_owned()];
        let index = write_index(out.path(), Some(&chosen), Some(&set), None).unwrap();
        assert_eq!(index.tree, out.path().join(".branchyard/tools"));
        assert_eq!(index.chosen, ["find-tools"]);
        assert!(index.mounted.contains(&"pptx".to_owned()));
        assert!(index.mounted.contains(&"repository:style".to_owned()));
        let text = std::fs::read_to_string(index.tree.join("INDEX.md")).unwrap();
        assert!(text.contains("- find-tools:"), "{text}");
        assert!(
            text.contains(&format!("- ...{} more:", index.mounted.len())),
            "{text}"
        );
        assert!(text.len() <= crate::tooltree::INDEX_BUDGET);
        let hits = search(
            &index.tree,
            &["release".into(), "notes".into(), "dry".into()],
            5,
            false,
        )
        .unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits.iter().any(
                |h| h.id == "repository:style" || h.id == "repository:style/references/tone.md"
            ),
            "{hits:?}"
        );
        assert!(hits.iter().all(|h| !h.kind.is_empty()));
        let none = search(&index.tree, &["qzxvwk".into()], 5, true).unwrap();
        assert!(none.is_empty());
        // Every built-in skill is chosen without a choice.
        let all = write_index(&out.path().join("all"), None, None, None).unwrap();
        assert!(all.mounted.is_empty());
        assert_eq!(
            all.chosen.len(),
            crate::capabilities::catalogued().unwrap().len()
        );
        assert_eq!(
            parse_hit(
                "skill      docx: Create, inspect and validate editable Word DOCX documents."
            ),
            Some(Hit {
                kind: "skill".into(),
                id: "docx".into(),
                description: "Create, inspect and validate editable Word DOCX documents.".into(),
            })
        );
        assert_eq!(parse_hit("connector  github").unwrap().description, "");
        let missing = search(&out.path().join("nowhere"), &["x".into()], 5, false);
        assert!(missing.is_err());
    }
}
