//! Source-free skills with progressive discovery for every task, independent of delegation.
//!
//! Before each turn the catalogued built-in skills, the repository's
//! skills when the branch stages them, and the person's own skills when
//! the branch was created with their folder (`crate::skills`), are written
//! under `.branchyard/capabilities/` of the branch's private home, or of
//! its cache under the state directory when it has none ([`cache`]).
//! Repository skills go in `repository/<name>/` and the person's in
//! `user/<name>/`, beside the built-ins, and are listed under a generated
//! Custom topic; files a set no longer has are removed, so the tree is the
//! turn's set whatever an earlier turn left.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use branchyard_provision::skillset::{self, SkillEntry};
use branchyard_provision::{apply, built_in, skills, Edit, Plan};
use serde::Deserialize;

use crate::skills::{RepositorySet, UserSet};
use crate::tooltree::Tool;

pub(crate) const DIRECTORY: &str = ".branchyard/capabilities";

#[derive(Deserialize)]
struct Catalog {
    skills: Vec<Capability>,
}

#[derive(Deserialize)]
struct Capability {
    id: String,
}

#[derive(Debug)]
pub(crate) struct Staged {
    /// Files written or removed, relative to the home.
    pub written: Vec<String>,
    /// The built-in skills staged, in catalog order: the chosen ones, and
    /// where every ready skill is mounted the rest the policy allows.
    pub built_in: Vec<SkillEntry>,
    /// The skills the turn chose, which the tool tree's index names
    /// ([`crate::tooltree`]) and the harness's native skills directory
    /// links: the built-in skills the task picked, or every staged
    /// built-in when it picked none.
    pub chosen: Vec<Tool>,
    /// The skills staged beside them and not chosen, which the index only
    /// counts (`docs/skill-registry.md`, R2): built-in skills the policy
    /// allows and the task did not pick, where every ready skill is
    /// mounted; then the repository's trusted skills and the person's own,
    /// which are ready, not chosen, wherever they are staged.
    pub mounted: Vec<Tool>,
}

impl Staged {
    /// Every staged skill, chosen then mounted: built-in, then the
    /// repository's, then the person's own.
    #[cfg(test)]
    pub fn listed(&self) -> Vec<Tool> {
        self.chosen.iter().chain(&self.mounted).cloned().collect()
    }
}

/// Which built-in skills a turn stages and which it names
/// (`docs/skill-registry.md`, R2).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Choice<'a> {
    /// The built-in skills the task chose ([`crate::Provisioning::skills`]),
    /// each with the skills it needs; `None` chooses every staged one.
    pub chosen: Option<&'a [String]>,
    /// The built-in skills the policy allows
    /// ([`crate::Provisioning::allowed_skills`]); `None` allows all.
    pub allowed: Option<&'a [String]>,
    /// Whether every allowed skill is staged, chosen or not: where the
    /// provider reads the tree in place or links the store
    /// ([`crate::providers::ProviderKind::mounts_ready_skills`]). Else the
    /// chosen skills alone are staged, as before R2.
    pub mount_ready: bool,
}

impl Choice<'_> {
    /// The built-in skills staged for this choice, by name; `None` is every
    /// one. Each set is closed over the skills its members need.
    pub fn staged(&self) -> Result<Option<BTreeSet<String>>, String> {
        let allowed = self.allowed.map(built_in::closure).transpose()?;
        let chosen = self.chosen().transpose()?;
        Ok(match (self.mount_ready, chosen) {
            (true, _) | (false, None) => allowed,
            (false, Some(chosen)) => Some(chosen),
        })
    }

    /// The built-in skills the task chose, closed over what they need and
    /// held within what the policy allows; `None` is every staged one.
    pub fn chosen(&self) -> Option<Result<BTreeSet<String>, String>> {
        let chosen = self.chosen?;
        Some(built_in::closure(chosen).and_then(|set| {
            Ok(match self.allowed.map(built_in::closure).transpose()? {
                Some(allowed) => set.intersection(&allowed).cloned().collect(),
                None => set,
            })
        }))
    }
}

/// The directory of the state directory `state` holding the skills of
/// `branch`, a branch without a private home; removed with the branch.
pub(crate) fn cache(state: &Path, branch: &str) -> PathBuf {
    state.join("capabilities").join(branch_id(branch))
}

/// The name of `branch`'s cache directory: BLAKE3 of its name, in hex.
fn branch_id(branch: &str) -> String {
    blake3::hash(branch.as_bytes()).to_hex().to_string()
}

/// A separate cache per branch avoids concurrent writers in a shared HOME.
pub(crate) fn local_home(state: &Path, branch: &str) -> Result<PathBuf, String> {
    let mut path = state
        .canonicalize()
        .map_err(|e| format!("could not resolve document skill cache root: {e}"))?;
    let branch_id = branch_id(branch);
    for name in ["capabilities", branch_id.as_str()] {
        path.push(name);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("could not create document skill cache: {error}")),
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("could not inspect document skill cache: {e}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "document skill cache {} is not a plain directory; refusing to follow symbolic links",
                path.display()
            ));
        }
    }
    Ok(path)
}

/// The built-in skills every turn stages, in catalog order.
pub(crate) fn catalogued() -> Result<Vec<&'static skills::Skill>, String> {
    let catalog: Catalog = serde_json::from_str(skills::CAPABILITIES)
        .map_err(|e| format!("invalid embedded capabilities: {e}"))?;
    catalog
        .skills
        .iter()
        .map(|capability| {
            skills::skill(&capability.id)
                .ok_or_else(|| format!("embedded skill {} is missing", capability.id))
        })
        .collect()
}

/// Install the embedded payload, and `repository`'s and the person's
/// `user` skills when given, on the host.
///
/// `only` is the task's choice of built-in skills
/// ([`crate::Provisioning::skills`]): those and the skills they need are
/// staged and chosen, the rest removed, and the indexes list only what was
/// staged. `None` stages every built-in skill.
#[cfg(test)]
pub(crate) fn stage(
    host: &Path,
    repository: Option<&RepositorySet>,
    user: Option<&UserSet>,
    only: Option<&[String]>,
) -> Result<Staged, String> {
    stage_from(
        host,
        repository,
        user,
        Choice {
            chosen: only,
            ..Choice::default()
        },
        None,
    )
}

/// [`stage`] for a [`Choice`], linking each built-in skill the shared tool
/// `store` has an entry for ([`crate::toolstore`]) instead of writing its
/// files: the skill's directory is a link to the entry. The caller passes
/// the store only when the harness sees it at the same path. A skill the
/// store lacks is written as before, and a link left by an earlier turn
/// that had the store is replaced by the files. With `choice.mount_ready`,
/// every allowed built-in skill is staged and the ones the task did not
/// choose are returned as mounted; the repository's and the person's skills
/// are staged and mounted wherever the turn runs.
pub(crate) fn stage_from(
    host: &Path,
    repository: Option<&RepositorySet>,
    user: Option<&UserSet>,
    choice: Choice<'_>,
    store: Option<&crate::toolstore::ToolStore>,
) -> Result<Staged, String> {
    let staged = choice.staged()?;
    let chosen = choice.chosen().transpose()?;
    let indexes = built_in::indexes(staged.as_ref())?;
    let mut plan = Plan::default();
    let mut built_in = Vec::new();
    let mut picked = Vec::new();
    let mut mounted = Vec::new();
    let mut links = Vec::new();
    let mut unlinked = Vec::new();
    for skill in catalogued()? {
        let directory = format!("{DIRECTORY}/{}", skill.name);
        let is_staged = staged.as_ref().is_none_or(|set| set.contains(skill.name));
        let entry = store
            .filter(|_| is_staged)
            .and_then(|store| store.skill(skill));
        match &entry {
            Some(entry) => links.push((directory.clone(), entry.clone())),
            None => unlinked.push(directory.clone()),
        }
        if !is_staged {
            // A narrower turn than the last removes what it does not give.
            for stale in files_under(host, &directory)? {
                plan.edit(&stale, false, Edit::Remove);
            }
            continue;
        }
        for (path, content) in skill.files {
            // With an entry the link stands for the files, and replaces
            // any copy an earlier turn made ([`link_skill`]).
            if entry.is_none() {
                plan.edit(
                    &format!("{directory}/{path}"),
                    false,
                    Edit::Put((*content).into()),
                );
            }
        }
        built_in.push(skillset::built_in_entry(skill));
        let tool = Tool {
            id: skill.name.to_owned(),
            description: description_of(skill)?,
            directory: skill.name.to_owned(),
        };
        match chosen.as_ref().is_none_or(|set| set.contains(skill.name)) {
            true => picked.push(tool),
            false => mounted.push(tool),
        }
    }
    let index_path = format!("{DIRECTORY}/INDEX.md");
    let custom_path = format!("{DIRECTORY}/{}", skillset::CUSTOM_TOPIC);
    let mut custom = BTreeSet::new();
    let repository_skills: &[skillset::Skill] = repository.map_or(&[], |set| &set.skills);
    let user_skills: &[skillset::Skill] = user.map_or(&[], |set| &set.skills);
    if repository_skills.is_empty() && user_skills.is_empty() {
        plan.edit(&index_path, false, Edit::Put(indexes.root.clone()));
        plan.edit(&custom_path, false, Edit::Remove);
    } else {
        let commit = repository.map_or("", |set| set.commit.as_str());
        let index = skillset::custom_index(&indexes.root, repository_skills, user_skills, commit)?;
        plan.edit(&index_path, false, Edit::Put(index.root));
        plan.edit(&custom_path, false, Edit::Put(index.topic));
    }
    for (source, skills) in [
        (skillset::Source::Repository, repository_skills),
        (skillset::Source::User, user_skills),
    ] {
        let source_dir = format!("{DIRECTORY}/{}", source.root());
        for skill in skills {
            // Ready by a person's trust or their own folder, never chosen
            // per query: the index counts them and search finds them.
            mounted.push(Tool {
                id: skillset::label(source, &skill.name),
                description: skill.description.clone(),
                directory: format!("{}/{}", source.root(), skill.name),
            });
            for (path, content) in &skill.files {
                let path = format!("{source_dir}/{}/{path}", skill.name);
                plan.edit(&path, false, Edit::Put(content.clone()));
                custom.insert(path);
            }
        }
        for stale in files_under(host, &source_dir)? {
            if !custom.contains(&stale) {
                plan.edit(&stale, false, Edit::Remove);
            }
        }
    }
    if !skills::UPSTREAM_LOCK.is_empty() {
        plan.edit(
            &format!("{DIRECTORY}/upstream.lock.json"),
            false,
            Edit::Put(skills::UPSTREAM_LOCK.into()),
        );
    }
    for (path, content) in indexes.topics {
        let edit = match content {
            Some(text) => Edit::Put(text),
            None => Edit::Remove,
        };
        plan.edit(&format!("{DIRECTORY}/{path}"), false, edit);
    }
    // A link an earlier turn made to a skill this turn writes, or drops, is
    // removed first: the files are never written through it.
    let fail = |e: io::Error| format!("could not link document capabilities: {e}");
    let mut linked = Vec::new();
    for directory in &unlinked {
        if unlink_skill(&host.join(directory)).map_err(fail)? {
            linked.push(directory.clone());
        }
    }
    let applied = apply::apply(&plan, host)
        .map_err(|e| format!("could not install document capabilities: {e}"))?;
    for (directory, entry) in &links {
        if link_skill(&host.join(directory), entry).map_err(fail)? {
            linked.push(directory.clone());
        }
    }
    Ok(Staged {
        written: applied
            .written
            .into_iter()
            .chain(applied.removed)
            .chain(linked)
            .collect(),
        built_in,
        chosen: picked,
        mounted,
    })
}

/// Make `path` a real directory whose entries are links to those of the
/// store's `entry`, replacing the copy or the other links there. The
/// directory itself is not a link so that a skill's relative references to
/// its siblings (`../artifact-tools/INDEX.md`) resolve beside it, not in
/// the store. Whether anything changed.
fn link_skill(path: &Path, entry: &Path) -> io::Result<bool> {
    let mut changed = false;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            std::fs::remove_file(path)?;
            changed = true;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(path)?;
    let mut wanted = BTreeSet::new();
    for child in std::fs::read_dir(entry)? {
        let name = child?.file_name();
        let (link, target) = (path.join(&name), entry.join(&name));
        match std::fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&link)? != target {
                    std::fs::remove_file(&link)?;
                    std::os::unix::fs::symlink(&target, &link)?;
                    changed = true;
                }
            }
            Ok(meta) => {
                match meta.is_dir() {
                    true => std::fs::remove_dir_all(&link)?,
                    false => std::fs::remove_file(&link)?,
                }
                std::os::unix::fs::symlink(&target, &link)?;
                changed = true;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::os::unix::fs::symlink(&target, &link)?;
                changed = true;
            }
            Err(e) => return Err(e),
        }
        wanted.insert(name);
    }
    // What the entry no longer holds, or never did, goes.
    for child in std::fs::read_dir(path)? {
        let child = child?;
        if !wanted.contains(&child.file_name()) && child.file_type()?.is_symlink() {
            std::fs::remove_file(child.path())?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Remove the links [`link_skill`] made in `dir`, and `dir` when nothing
/// else is in it: a skill the turn writes as files, or drops, must not be
/// written through a link into the store. Whether anything changed.
fn unlink_skill(dir: &Path) -> io::Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(false)
        }
        Err(e) => return Err(e),
    };
    let mut changed = false;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            std::fs::remove_file(entry.path())?;
            changed = true;
        }
    }
    // Left when something else is in it.
    if changed && std::fs::read_dir(dir)?.next().is_none() {
        std::fs::remove_dir(dir)?;
    }
    Ok(changed)
}

/// A built-in skill's description, from its `SKILL.md` frontmatter.
pub(crate) fn description_of(skill: &skills::Skill) -> Result<String, String> {
    let (_, text) = skill
        .files
        .iter()
        .find(|(path, _)| *path == "SKILL.md")
        .ok_or_else(|| format!("embedded skill {} has no SKILL.md", skill.name))?;
    let front = skillset::frontmatter(text)
        .map_err(|why| format!("embedded skill {}: {why}", skill.name))?;
    Ok(front
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

/// The regular files under `home`'s `dir`, relative to `home`, without
/// following symbolic links: none when `dir`, or a directory on the way to
/// it, is missing, a link or not a directory (the harness can write its
/// home; [`apply::apply`] then refuses to write through it). Removing them
/// goes through [`apply::apply`] too.
fn files_under(home: &Path, dir: &str) -> Result<Vec<String>, String> {
    let fail = |e: io::Error| format!("could not list the staged repository skills: {e}");
    let mut on_the_way = home.to_path_buf();
    for part in dir.split('/') {
        on_the_way.push(part);
        match std::fs::symlink_metadata(&on_the_way) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(fail(e)),
        }
    }
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(relative) = pending.pop() {
        let entries = match std::fs::read_dir(home.join(&relative)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(fail(e)),
        };
        for entry in entries {
            let entry = entry.map_err(fail)?;
            let kind = entry.file_type().map_err(fail)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = format!("{relative}/{name}");
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_free_payload_is_staged_listed_and_repeated_staging_is_unchanged() {
        let host = tempfile::tempdir().unwrap();
        let first = stage(host.path(), None, None, None).unwrap();
        assert!(!first.written.is_empty());
        assert_eq!(first.listed().len(), first.built_in.len());
        let listed = first.listed();
        let find = listed.iter().find(|t| t.id == "find-tools").unwrap();
        assert_eq!(find.directory, "find-tools");
        assert!(find.description.starts_with("Search the tools mounted"));
        let root = host.path().join(DIRECTORY);
        let index = std::fs::read_to_string(root.join("INDEX.md")).unwrap();
        assert_eq!(index, skills::INDEX);
        assert!(index.len() <= 4096);
        for (path, content) in skills::TOPIC_INDEXES {
            assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), *content);
        }
        let catalog: Catalog = serde_json::from_str(skills::CAPABILITIES).unwrap();
        for capability in catalog.skills {
            let directory = root.join(capability.id);
            assert!(directory.join("INDEX.md").is_file());
            assert!(directory.join("SKILL.md").is_file());
        }
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.file_name().is_some_and(|name| name == "INDEX.md") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert!(text.len() <= 8192);
                    for line in text.lines() {
                        let Some((_, target)) = line.split_once("](") else {
                            continue;
                        };
                        let relative = target.split_once(')').unwrap().0;
                        assert!(!Path::new(relative).is_absolute());
                        let resolved = path
                            .parent()
                            .unwrap()
                            .join(relative)
                            .canonicalize()
                            .unwrap();
                        assert!(resolved.starts_with(root.canonicalize().unwrap()));
                        assert!(resolved.is_file(), "{} links to {relative}", path.display());
                    }
                }
            }
        }
        assert!(!index.contains(&host.path().display().to_string()));
        let helper = host
            .path()
            .join(DIRECTORY)
            .join("artifact-tools/scripts/run.py");
        let output = branchyard_support::child::command("python3")
            .arg(helper)
            .arg("doctor")
            .env("HOME", host.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report.is_object());
        assert!(stage(host.path(), None, None, None)
            .unwrap()
            .written
            .is_empty());
    }

    /// A turn that chooses some built-in skills gets those and what they
    /// need, an index and a listing naming only them, and nothing an
    /// earlier, wider turn left; a later turn without a choice gets every
    /// skill back.
    #[test]
    fn a_chosen_set_stages_only_those_skills_and_their_needs() {
        let host = tempfile::tempdir().unwrap();
        let root = host.path().join(DIRECTORY);
        stage(host.path(), None, None, None).unwrap();
        assert!(root.join("docx/SKILL.md").is_file());
        let chosen = ["pptx".to_owned()];
        let narrow = stage(host.path(), None, None, Some(&chosen)).unwrap();
        let names: Vec<&str> = narrow.built_in.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["artifact-tools", "pptx"]);
        let staged_tools = narrow.listed();
        let listed: Vec<&str> = staged_tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(listed, ["artifact-tools", "pptx"]);
        assert!(!root.join("docx/SKILL.md").exists());
        assert!(!root.join("topics/design/INDEX.md").exists());
        let index = std::fs::read_to_string(root.join("INDEX.md")).unwrap();
        assert!(index.contains("topics/documents/INDEX.md"));
        assert!(!index.contains("topics/design/INDEX.md"));
        stage(host.path(), None, None, None).unwrap();
        assert!(root.join("docx/SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(root.join("INDEX.md")).unwrap(),
            skills::INDEX
        );
        let error = stage(host.path(), None, None, Some(&["nope".to_owned()])).unwrap_err();
        assert!(error.contains("\"nope\""), "{error}");
    }

    fn repository_set(texts: &[(&str, &str)]) -> RepositorySet {
        let skills = texts
            .iter()
            .map(|(name, extra)| {
                let mut files = vec![(
                    "SKILL.md".to_owned(),
                    format!("---\nname: {name}\ndescription: The {name} skill.\n---\n"),
                )];
                if !extra.is_empty() {
                    files.push(((*extra).to_owned(), "x".to_owned()));
                }
                skillset::Skill::new(name, files).unwrap()
            })
            .collect::<Vec<_>>();
        let entries: Vec<SkillEntry> = skills
            .iter()
            .map(|s| s.entry(skillset::Source::Repository))
            .collect();
        RepositorySet {
            commit: "0123456789abcdef".into(),
            digest: skillset::set_digest(&entries),
            skills,
        }
    }

    fn user_set(texts: &[(&str, &str)]) -> UserSet {
        UserSet {
            skills: repository_set(texts).skills,
            withheld: Vec::new(),
        }
    }

    #[test]
    fn repository_skills_are_staged_beside_the_built_ins_and_what_a_set_lost_goes() {
        let host = tempfile::tempdir().unwrap();
        let root = host.path().join(DIRECTORY);
        let set = repository_set(&[("style", "references/a.md"), ("tone", "")]);
        let first = stage(host.path(), Some(&set), None, None).unwrap();
        assert_eq!(first.built_in.len(), catalogued().unwrap().len());
        assert!(root.join("repository/style/references/a.md").is_file());
        assert!(root.join("repository/tone/SKILL.md").is_file());
        let index = std::fs::read_to_string(root.join("INDEX.md")).unwrap();
        assert!(index.contains("[Custom](topics/custom/INDEX.md)"));
        assert!(root.join(skillset::CUSTOM_TOPIC).is_file());
        assert!(stage(host.path(), Some(&set), None, None)
            .unwrap()
            .written
            .is_empty());

        let smaller = repository_set(&[("style", "")]);
        let second = stage(host.path(), Some(&smaller), None, None).unwrap();
        assert!(!root.join("repository/style/references/a.md").exists());
        assert!(!root.join("repository/tone/SKILL.md").exists());
        assert!(second
            .written
            .contains(&format!("{DIRECTORY}/repository/tone/SKILL.md")));

        let none = stage(host.path(), None, None, None).unwrap();
        assert!(!root.join("repository/style/SKILL.md").exists());
        assert!(!root.join(skillset::CUSTOM_TOPIC).exists());
        assert_eq!(
            std::fs::read_to_string(root.join("INDEX.md")).unwrap(),
            skills::INDEX
        );
        assert!(none
            .written
            .contains(&format!("{DIRECTORY}/{}", skillset::CUSTOM_TOPIC)));
    }

    #[test]
    fn a_persons_skills_are_staged_under_user_beside_the_repositorys() {
        let host = tempfile::tempdir().unwrap();
        let root = host.path().join(DIRECTORY);
        let mine = user_set(&[("memory-parser", "references/bench.md")]);
        let alone = stage(host.path(), None, Some(&mine), None).unwrap();
        assert!(root
            .join("user/memory-parser/references/bench.md")
            .is_file());
        assert!(!root.join("repository").exists());
        let topic = std::fs::read_to_string(root.join(skillset::CUSTOM_TOPIC)).unwrap();
        assert!(topic.starts_with("# Custom\n\nSkills the person running this branch"));
        assert!(topic.contains("- [user:memory-parser](../../user/memory-parser/SKILL.md)"));
        let alone_tools = alone.listed();
        let ids: Vec<&str> = alone_tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.last(), Some(&"user:memory-parser"));
        assert_eq!(
            alone.listed().last().unwrap().directory,
            "user/memory-parser"
        );
        assert!(alone
            .written
            .contains(&format!("{DIRECTORY}/user/memory-parser/SKILL.md")));

        let set = repository_set(&[("style", "")]);
        let both = stage(host.path(), Some(&set), Some(&mine), None).unwrap();
        let topic = std::fs::read_to_string(root.join(skillset::CUSTOM_TOPIC)).unwrap();
        assert!(topic.contains("as of commit 0123456789ab, and skills the person"));
        let both_tools = both.listed();
        let ids: Vec<&str> = both_tools.iter().map(|t| t.id.as_str()).collect();
        let style_at = ids.iter().position(|id| *id == "repository:style").unwrap();
        assert_eq!(ids[style_at + 1], "user:memory-parser", "{ids:?}");
        assert!(root.join("repository/style/SKILL.md").is_file());

        // A folder that lost the skill loses its files; the directory goes.
        let fewer = stage(host.path(), Some(&set), Some(&UserSet::default()), None).unwrap();
        assert!(!root.join("user/memory-parser/SKILL.md").exists());
        assert!(fewer
            .written
            .contains(&format!("{DIRECTORY}/user/memory-parser/SKILL.md")));
        let topic = std::fs::read_to_string(root.join(skillset::CUSTOM_TOPIC)).unwrap();
        assert!(!topic.contains("user:"));
        assert!(stage(host.path(), Some(&set), None, None)
            .unwrap()
            .written
            .is_empty());
    }

    #[test]
    fn a_linked_repository_directory_in_the_home_is_not_walked() {
        let host = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), "mine").unwrap();
        stage(host.path(), None, None, None).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            host.path().join(DIRECTORY).join(skillset::REPOSITORY_ROOT),
        )
        .unwrap();
        assert!(files_under(host.path(), &format!("{DIRECTORY}/repository"))
            .unwrap()
            .is_empty());
        stage(host.path(), None, None, None).unwrap();
        assert!(outside.path().join("keep.txt").is_file());
    }

    #[test]
    fn branch_caches_are_distinct_and_symlink_directories_are_refused() {
        let state = tempfile::tempdir().unwrap();
        assert_ne!(
            local_home(state.path(), "a").unwrap(),
            local_home(state.path(), "b").unwrap()
        );
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), state.path().join(".branchyard")).unwrap();
        assert!(stage(state.path(), None, None, None)
            .unwrap_err()
            .contains("symbolic link"));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn local_cache_rejects_linked_ancestors_and_accepts_a_trusted_outer_root() {
        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), state.path().join("capabilities")).unwrap();
        assert!(local_home(state.path(), "branch")
            .unwrap_err()
            .contains("symbolic links"));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("state")).unwrap();
        let home = local_home(&linked.path().join("state"), "branch").unwrap();
        assert!(home.starts_with(outside.path().canonicalize().unwrap()));
        assert!(stage(&home, None, None, None).is_ok());
        assert_eq!(
            local_home(&linked.path().join("state"), "branch").unwrap(),
            home
        );
    }
    /// With the shared store, each chosen built-in skill's files are links
    /// to the store's entry, in a directory of the turn's own so that its
    /// references to sibling skills resolve; a skill the store lacks, or a
    /// turn without the store, gets files, and nothing is written through
    /// a link left by an earlier turn.
    #[test]
    fn skills_the_store_has_are_linked_and_the_rest_are_copied() {
        use crate::toolstore::{publish, ToolStore};
        let shared = branchyard_testkit::Scratch::new("capabilities-store");
        publish(shared.path(), None).unwrap();
        let store = ToolStore::new(shared.path());
        let host = branchyard_testkit::Scratch::new("capabilities-host");
        let root = host.path().join(DIRECTORY);
        let linked = |name: &str| {
            let skill = root.join(name);
            let meta = std::fs::symlink_metadata(&skill).unwrap();
            assert!(meta.is_dir() && !meta.file_type().is_symlink(), "{name}");
            std::fs::read_link(skill.join("SKILL.md")).ok()
        };
        let first = stage_from(host.path(), None, None, Choice::default(), Some(&store)).unwrap();
        assert_eq!(first.listed().len(), first.built_in.len());
        for tool in &first.listed() {
            let link = linked(&tool.directory).unwrap();
            assert!(link.starts_with(shared.path().join("skills")), "{link:?}");
            assert!(link.is_file());
        }
        // A sibling reference resolves beside the skill, not in the store.
        assert!(root
            .join("source-capture/../artifact-tools/INDEX.md")
            .is_file());
        assert!(std::fs::read_to_string(root.join("find-tools/SKILL.md"))
            .unwrap()
            .contains("name: find-tools"));
        assert_eq!(
            std::fs::read_to_string(root.join("INDEX.md")).unwrap(),
            skills::INDEX
        );
        assert!(
            stage_from(host.path(), None, None, Choice::default(), Some(&store))
                .unwrap()
                .written
                .is_empty()
        );

        // An entry the store lacks is copied beside the links.
        let digest = crate::toolstore::skill_digest(skills::skill("pptx").unwrap());
        std::fs::remove_dir_all(shared.join(format!("skills/pptx@{digest}"))).unwrap();
        let partial = stage_from(host.path(), None, None, Choice::default(), Some(&store)).unwrap();
        assert_eq!(linked("pptx"), None);
        assert!(root.join("pptx/SKILL.md").is_file());
        assert!(partial
            .written
            .iter()
            .any(|w| w.starts_with(".branchyard/capabilities/pptx")));
        assert!(linked("docx").is_some());

        // A narrower turn drops the links of the skills it does not give.
        let chosen = ["docx".to_owned()];
        stage_from(
            host.path(),
            None,
            None,
            Choice {
                chosen: Some(&chosen),
                ..Choice::default()
            },
            Some(&store),
        )
        .unwrap();
        assert!(linked("docx").is_some());
        assert!(!root.join("pptx/SKILL.md").exists());
        assert!(!root.join("xlsx/SKILL.md").exists());
        assert!(!root.join("xlsx/SKILL.md").is_symlink());

        // A turn on a provider that does not see the store gets files, and
        // the store is not written through the links it replaces.
        let before = crate::connectors::packager::hash_dir(&shared.join("skills")).unwrap();
        let copied = stage_from(host.path(), None, None, Choice::default(), None).unwrap();
        assert!(!copied.written.is_empty());
        for tool in &copied.listed() {
            assert_eq!(linked(&tool.directory), None, "{}", tool.id);
            assert!(root.join(&tool.directory).join("SKILL.md").is_file());
        }
        assert_eq!(
            crate::connectors::packager::hash_dir(&shared.join("skills")).unwrap(),
            before
        );
        assert!(stage_from(host.path(), None, None, Choice::default(), None)
            .unwrap()
            .written
            .is_empty());
    }

    /// Where every ready skill is mounted, a turn that chooses `pptx` still
    /// stages every built-in skill the policy allows: `pptx` and what it
    /// needs are chosen, the rest are mounted; a policy that allows only
    /// some built-ins narrows what is staged; and the layered indexes list
    /// what is staged. A later chosen-only turn drops the rest again.
    #[test]
    fn mounting_every_ready_skill_stages_the_allowed_built_ins_and_chooses_the_picked_ones() {
        let host = tempfile::tempdir().unwrap();
        let root = host.path().join(DIRECTORY);
        let chosen = ["pptx".to_owned()];
        let all = stage_from(
            host.path(),
            None,
            None,
            Choice {
                chosen: Some(&chosen),
                allowed: None,
                mount_ready: true,
            },
            None,
        )
        .unwrap();
        let picked: Vec<&str> = all.chosen.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(picked, ["artifact-tools", "pptx"]);
        assert_eq!(all.built_in.len(), catalogued().unwrap().len());
        assert_eq!(all.mounted.len(), all.built_in.len() - 2);
        assert!(all.mounted.iter().any(|t| t.id == "docx"));
        assert!(all.mounted.iter().all(|t| t.id != "pptx"));
        assert!(root.join("docx/SKILL.md").is_file(), "mounted, so staged");
        assert_eq!(
            std::fs::read_to_string(root.join("INDEX.md")).unwrap(),
            skills::INDEX,
            "the layered index lists what is staged"
        );

        // A policy that allows three skills: the chosen one, its need, and one more mounted.
        let allowed = ["pptx".to_owned(), "docx".to_owned()];
        let narrowed = stage_from(
            host.path(),
            None,
            None,
            Choice {
                chosen: Some(&chosen),
                allowed: Some(&allowed),
                mount_ready: true,
            },
            None,
        )
        .unwrap();
        let names: Vec<&str> = narrowed.built_in.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["artifact-tools", "docx", "pptx"]);
        let mounted: Vec<&str> = narrowed.mounted.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(mounted, ["docx"]);
        assert!(
            !root.join("xlsx/SKILL.md").exists(),
            "not allowed, not staged"
        );
        // No choice: every allowed skill is chosen, none merely mounted.
        let unchosen = stage_from(
            host.path(),
            None,
            None,
            Choice {
                chosen: None,
                allowed: Some(&allowed),
                mount_ready: true,
            },
            None,
        )
        .unwrap();
        assert_eq!(unchosen.chosen.len(), 3);
        assert!(unchosen.mounted.is_empty());

        // A copied home stays chosen-only: the picked skills and nothing else.
        let only = stage(host.path(), None, None, Some(&chosen)).unwrap();
        assert_eq!(only.built_in.len(), 2);
        assert!(only.mounted.is_empty());
        assert!(!root.join("docx/SKILL.md").exists());
    }

    /// The repository's and the person's skills are mounted, not chosen,
    /// wherever they are staged: the index counts them and search finds them.
    #[test]
    fn custom_skills_are_mounted_not_chosen() {
        let host = tempfile::tempdir().unwrap();
        let set = repository_set(&[("style", "")]);
        let mine = user_set(&[("memory-parser", "")]);
        for mount_ready in [false, true] {
            let staged = stage_from(
                host.path(),
                Some(&set),
                Some(&mine),
                Choice {
                    chosen: Some(&["docx".to_owned()]),
                    allowed: None,
                    mount_ready,
                },
                None,
            )
            .unwrap();
            let chosen: Vec<&str> = staged.chosen.iter().map(|t| t.id.as_str()).collect();
            assert_eq!(chosen, ["artifact-tools", "docx"], "docx and what it needs");
            let mounted: Vec<&str> = staged.mounted.iter().map(|t| t.id.as_str()).collect();
            assert!(
                mounted.ends_with(&["repository:style", "user:memory-parser"]),
                "{mounted:?}"
            );
            assert_eq!(mounted.contains(&"pptx"), mount_ready);
        }
    }

    #[test]
    fn a_choice_closes_over_needs_and_stays_within_the_allowed_set() {
        let chosen = ["pptx".to_owned()];
        let allowed = ["pptx".to_owned()];
        let choice = Choice {
            chosen: Some(&chosen),
            allowed: Some(&allowed),
            mount_ready: true,
        };
        // `pptx` needs `artifact-tools`, which the allowed set brings with it.
        let staged = choice.staged().unwrap().unwrap();
        assert!(staged.contains("artifact-tools") && staged.contains("pptx"));
        let picked = choice.chosen().unwrap().unwrap();
        assert_eq!(picked, staged);
        assert!(Choice::default().staged().unwrap().is_none());
        assert!(Choice::default().chosen().is_none());
        let bad = ["nope".to_owned()];
        assert!(Choice {
            chosen: Some(&bad),
            ..Choice::default()
        }
        .staged()
        .is_err());
    }
}
