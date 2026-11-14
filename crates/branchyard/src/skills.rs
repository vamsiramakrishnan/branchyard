//! The repository's own skills: `.agents/skills/<name>/`, read from a
//! commit with git and never from a worktree, so what an agent edits or
//! commits during its turn is not a skill for its next turn or its
//! children.
//!
//! A branch's repository skills are fixed when it is created
//! ([`BranchSkills`]): the commit they are read from (its base), their
//! entries and set digest, and whether they are staged or withheld and
//! why. Whether they are staged is the creating caller's decision
//! ([`SkillGate`], [`crate::TaskOptions::repository_skills`]): `by` stages
//! a set a person trusted by its digest (`by skills trust`), a server one
//! its operator allows (`allow_repository_skills`), and the SDK nothing
//! unless asked. Forks, reincarnations and delegated children inherit
//! their parent's. Each turn reads the staged ones again from that commit
//! and checks their digest (`for_turn`); `crate::capabilities` stages them
//! beside the built-in skills, and every turn's `Provisioned` activity
//! names the whole set ([`SkillSet`]). See `docs/knowledge-skills.md`.
//!
//! A person's own skills, the local `user` source of the skill registry
//! (`docs/skill-registry.md`), are the directories of their skills folder
//! (`~/.config/branchyard/skills/<name>/`, [`USER_SKILLS`] below the user
//! configuration directory; `by skills add` copies a bundle there, `by
//! memory add --mine` writes a memory bundle there). They are not fixed
//! with the branch: a turn reads the folder the branch was created with
//! again each time (`user_for_turn`), stages what it holds under
//! `user/<name>/` and records each skill's digest, so what a turn saw is
//! known from its record. A repository's staged skill shadows a user
//! skill of the same name, and a built-in's name stays reserved; a skill
//! the folder holds and the turn does not stage is withheld by name with
//! its reason ([`SkillSet::user_withheld`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use branchyard_provision::skillset;
pub use branchyard_provision::skillset::{
    frontmatter, reserved, Frontmatter, Skill, SkillEntry, Source,
};
use branchyard_workspace::Git;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Where a repository keeps its skills, one directory each.
pub const REPOSITORY_SKILLS: &str = ".agents/skills";

/// Where a person keeps their own skills, below the user configuration
/// directory (`~/.config/branchyard/skills/`), one directory each.
pub const USER_SKILLS: &str = "skills";

/// Why a branch has no repository skills when its creator took no
/// decision: [`SkillGate`]'s default.
const NOT_ALLOWED: &str = "the branch's creator did not allow repository skills";

/// Whether a new branch stages the repository's skills from its base
/// commit. Read only when a branch is created from a revision (run, fan);
/// forks, reincarnations and delegated children inherit their parent's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillGate {
    /// Not staged; the reason is recorded with every turn's skills.
    Withheld(String),
    /// Staged when the set at the base commit has this set digest, the one
    /// a person trusted (`by skills trust`); withheld as changed otherwise.
    Trusted(String),
    /// Staged as found: an operator's decision, as a server's
    /// `allow_repository_skills`.
    Allowed,
}

impl Default for SkillGate {
    /// Withheld, saying the branch's creator did not allow them.
    fn default() -> Self {
        SkillGate::Withheld(NOT_ALLOWED.into())
    }
}

/// The repository's skills at one commit: checked, within the index's
/// budgets, and in name order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositorySet {
    /// The commit they were read from.
    pub commit: String,
    /// Each skill, in name order.
    pub skills: Vec<Skill>,
    /// [`skillset::set_digest`] of their entries: what a person trusts.
    pub digest: String,
}

impl RepositorySet {
    /// Each skill's entry, from the repository source.
    pub fn entries(&self) -> Vec<SkillEntry> {
        self.skills
            .iter()
            .map(|skill| skill.entry(Source::Repository))
            .collect()
    }
}

/// A skill found and not staged, and why.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithheldSkill {
    /// Its directory's name.
    pub name: String,
    /// Why it was not staged.
    pub reason: String,
}

/// A person's own skills as one turn reads them from their folder: those
/// staged, checked and in name order, and those withheld by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserSet {
    /// The skills staged under `user/<name>/`, in name order.
    pub skills: Vec<Skill>,
    /// The directories of the folder that are not staged, and why.
    pub withheld: Vec<WithheldSkill>,
}

impl UserSet {
    /// Each staged skill's entry, from the user source.
    pub fn entries(&self) -> Vec<SkillEntry> {
        self.skills
            .iter()
            .map(|skill| skill.entry(Source::User))
            .collect()
    }
}

/// What a commit holds under [`REPOSITORY_SKILLS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// No skill directory.
    Nothing,
    /// Skills that cannot be used, and why.
    Refused(String),
    /// Skills that can.
    Skills(RepositorySet),
}

/// The repository skills a branch runs with, fixed when it was created and
/// inherited by its forks and children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSkills {
    /// The commit they are read from, the base of the branch that fixed
    /// them; never a worktree.
    pub commit: String,
    /// The skills found there; empty when they were refused.
    pub entries: Vec<SkillEntry>,
    /// [`skillset::set_digest`] of `entries`.
    pub digest: String,
    /// Why they are not staged; `None` when they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
}

/// The skills a turn ran with, as its `Provisioned` activity records them
/// on every turn, changed on disk or not.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSet {
    /// BLAKE3 over `entries`' sources, names and digests, in source and
    /// name order.
    pub set: String,
    /// Every skill staged for the turn: built-in, the repository's and
    /// the person's own (`user:` labels).
    pub entries: Vec<SkillEntry>,
    /// The repository's skills that were found and not staged, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<WithheldSkills>,
    /// The person's own skills that were found and not staged, each with
    /// why: a bad bundle, a built-in's name, or a repository skill of the
    /// same name staged before it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_withheld: Vec<WithheldSkill>,
}

/// Repository skills a turn did not stage.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithheldSkills {
    /// Those found; empty when they were refused.
    pub entries: Vec<SkillEntry>,
    /// Why they were not staged.
    pub reason: String,
}

/// What a turn stages from the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ForTurn {
    /// The branch has no repository skills.
    Nothing,
    /// These, read again from the branch's commit.
    Staged(RepositorySet),
    /// None of them, and why.
    Withheld(WithheldSkills),
}

/// The built-in skills every turn stages, each from the built-in source,
/// in catalog order.
pub fn built_in() -> Result<Vec<SkillEntry>, Error> {
    Ok(crate::capabilities::catalogued()
        .map_err(Error::State)?
        .into_iter()
        .map(skillset::built_in_entry)
        .collect())
}

/// The first 12 characters of a digest or commit, for messages.
fn short(text: &str) -> &str {
    text.get(..12).unwrap_or(text)
}

/// One entry of `git ls-tree -r -l` under [`REPOSITORY_SKILLS`].
struct Listed {
    mode: String,
    object: String,
    size: u64,
    /// The path below [`REPOSITORY_SKILLS`], with any byte that is not
    /// UTF-8 replaced.
    path: String,
    /// Whether the path was UTF-8 as committed.
    utf8: bool,
}

/// git in `root` for reading skills, with replace refs ignored: any
/// worktree of the repository can write `refs/replace/`, which would
/// otherwise change what a commit holds.
fn git(root: &Path) -> Git {
    Git::new(root).env("GIT_NO_REPLACE_OBJECTS", "1")
}

/// Read and check the skills under [`REPOSITORY_SKILLS`] at `rev` in the
/// repository at `root`, ignoring replace refs. A skill is a directory there
/// with a `SKILL.md`; other directories and loose files are not skills and
/// are skipped. A symbolic link or submodule in place of a directory or
/// inside a skill, a path or file that is not UTF-8, a skill a check
/// refuses and a set that would not fit the index refuse the whole set.
pub fn read(root: &Path, rev: &str) -> Result<Found, Error> {
    let commit = git(root)
        .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .run()
        .map_err(|_| Error::Git(format!("{rev:?} does not name a commit")))?
        .trim()
        .to_owned();
    let listing = git(root)
        .args([
            "ls-tree",
            "-r",
            "-z",
            "-l",
            "--full-tree",
            &commit,
            "--",
            REPOSITORY_SKILLS,
        ])
        .run_bytes()
        .map_err(crate::git::error)?;
    let listed = parse_listing(&listing)?;
    let refused = |why: String| {
        Ok(Found::Refused(format!(
            "the repository's skills at {} are refused: {why}",
            short(&commit)
        )))
    };
    if let Some(entry) = listed.iter().find(|entry| entry.path.is_empty()) {
        let what = match entry.mode.as_str() {
            "120000" => "a symbolic link",
            _ => "a file",
        };
        return refused(format!("{REPOSITORY_SKILLS} is {what}, not a directory"));
    }
    let mut skills: BTreeMap<&str, Vec<(&str, &Listed)>> = BTreeMap::new();
    for entry in &listed {
        match (entry.path.split_once('/'), entry.mode.as_str()) {
            (Some((name, rest)), _) => skills.entry(name).or_default().push((rest, entry)),
            (None, "120000") => {
                return refused(format!(
                    "{REPOSITORY_SKILLS}/{} is a symbolic link; skills are read only from the \
                     commit's own directories",
                    entry.path
                ))
            }
            (None, "160000") => {
                return refused(format!(
                    "{REPOSITORY_SKILLS}/{} is a submodule; skills are read only from the \
                     commit's own directories",
                    entry.path
                ))
            }
            // A loose file is not a skill.
            (None, _) => {}
        }
    }
    skills.retain(|_, files| files.iter().any(|(path, _)| *path == "SKILL.md"));
    if skills.is_empty() {
        return Ok(Found::Nothing);
    }
    // Each skill's index line takes at least this many bytes, so more
    // skills than this never fit, whatever their files say.
    let fewest = "- [repository:x](../../repository/x/SKILL.md): x (added by the repository's \
                  writers)\n"
        .len();
    if skills.len() * fewest > skillset::DETAIL_INDEX_BUDGET {
        return refused(format!(
            "{} skills cannot fit the skill index's {} bytes",
            skills.len(),
            skillset::DETAIL_INDEX_BUDGET
        ));
    }
    for (name, files) in &skills {
        for (path, entry) in files {
            if !entry.utf8 {
                return refused(format!("skill {name}: the path {path:?} is not UTF-8"));
            }
            match entry.mode.as_str() {
                "100644" | "100755" => {}
                "120000" => {
                    return refused(format!(
                        "skill {name}: {path} is a symbolic link, which a skill may not hold"
                    ))
                }
                _ => {
                    return refused(format!(
                        "skill {name}: {path} is not a regular file (mode {})",
                        entry.mode
                    ))
                }
            }
        }
        if let Err(why) =
            skillset::check_listing(name, files.iter().map(|(path, entry)| (*path, entry.size)))
        {
            return refused(why);
        }
    }
    let objects: Vec<&str> = skills
        .values()
        .flatten()
        .map(|(_, entry)| entry.object.as_str())
        .collect();
    let read = read_objects(root, &objects)?;
    let mut checked = Vec::new();
    for (name, files) in skills {
        let mut texts = Vec::new();
        for (path, entry) in files {
            let range = read.content.get(&entry.object).cloned().ok_or_else(|| {
                Error::Git(format!(
                    "git cat-file did not print object {}",
                    entry.object
                ))
            })?;
            let Some(text) = read
                .printed
                .get(range)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
            else {
                return refused(format!(
                    "skill {name}: {path} is not UTF-8 text; skills hold text files only"
                ));
            };
            texts.push((path.to_owned(), text.to_owned()));
        }
        match Skill::new(name, texts) {
            Ok(skill) => checked.push(skill),
            Err(why) => return refused(why),
        }
    }
    if let Err(why) =
        skillset::custom_index(branchyard_provision::skills::INDEX, &checked, &[], &commit)
    {
        return refused(why);
    }
    let digest = skillset::set_digest(
        &checked
            .iter()
            .map(|skill| skill.entry(Source::Repository))
            .collect::<Vec<_>>(),
    );
    Ok(Found::Skills(RepositorySet {
        commit,
        skills: checked,
        digest,
    }))
}

/// Parse `git ls-tree -r -z -l` output: `<mode> <type> <object> <size>\t<path>`
/// records, each ended by a NUL, keeping the path below
/// [`REPOSITORY_SKILLS`] (empty for that path itself).
fn parse_listing(listing: &[u8]) -> Result<Vec<Listed>, Error> {
    let malformed = || Error::Git("git ls-tree printed a line it should not".into());
    let mut entries = Vec::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(malformed)?;
        let meta = std::str::from_utf8(&record[..tab]).map_err(|_| malformed())?;
        let raw = &record[tab + 1..];
        let utf8 = std::str::from_utf8(raw).is_ok();
        let path = String::from_utf8_lossy(raw);
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(_kind), Some(object), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(malformed());
        };
        let below = match path.strip_prefix(REPOSITORY_SKILLS) {
            Some("") => String::new(),
            Some(rest) => rest.strip_prefix('/').ok_or_else(malformed)?.to_owned(),
            None => return Err(malformed()),
        };
        entries.push(Listed {
            mode: mode.to_owned(),
            object: object.to_owned(),
            // A submodule's size is `-`; its mode refuses it anyway.
            size: size.parse().unwrap_or(0),
            path: below,
            utf8,
        });
    }
    Ok(entries)
}

/// What `git cat-file --batch` printed, and where in it each object's
/// content is, by object name.
struct Objects {
    printed: Vec<u8>,
    content: BTreeMap<String, std::ops::Range<usize>>,
}

/// `objects`, through one `git cat-file --batch`.
fn read_objects(root: &Path, objects: &[&str]) -> Result<Objects, Error> {
    let mut input = String::new();
    for object in objects {
        input.push_str(object);
        input.push('\n');
    }
    let out = git(root)
        .args(["cat-file", "--batch"])
        .stdin(input)
        .run_bytes()
        .map_err(crate::git::error)?;
    let malformed = || Error::Git("git cat-file printed what it should not".into());
    let mut contents = BTreeMap::new();
    let mut at = 0;
    while at < out.len() {
        let end = out[at..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|offset| at + offset)
            .ok_or_else(malformed)?;
        let header = std::str::from_utf8(&out[at..end]).map_err(|_| malformed())?;
        let mut fields = header.split(' ');
        let (Some(object), Some("blob"), Some(size)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(Error::Git(format!("git cat-file could not read {header}")));
        };
        let size: usize = size.parse().map_err(|_| malformed())?;
        let body = end + 1..end + 1 + size;
        if body.end > out.len() {
            return Err(malformed());
        }
        contents.insert(object.to_owned(), body.clone());
        at = body.end + 1;
    }
    Ok(Objects {
        printed: out,
        content: contents,
    })
}

/// The repository skills a branch created at `base` runs with, under
/// `gate`; `None` when `base` has none. Skills that are refused, or that
/// cannot be read, are withheld with the reason, after the gate's own when
/// it withholds them anyway: a repository's skills never stop a branch
/// from being created.
pub(crate) fn fix(root: &Path, base: &str, gate: &SkillGate) -> Option<BranchSkills> {
    let refused = |why: String| {
        let reason = match gate {
            SkillGate::Withheld(gate) => format!("{gate}; and {why}"),
            SkillGate::Trusted(_) | SkillGate::Allowed => why,
        };
        Some(BranchSkills {
            commit: base.to_owned(),
            entries: Vec::new(),
            digest: skillset::set_digest(&[]),
            withheld: Some(reason),
        })
    };
    let set = match read(root, base) {
        Ok(Found::Nothing) => return None,
        Ok(Found::Refused(why)) => return refused(why),
        Err(error) => {
            return refused(format!(
                "the repository's skills could not be read at commit {}: {error}",
                short(base)
            ))
        }
        Ok(Found::Skills(set)) => set,
    };
    let withheld = match gate {
        SkillGate::Allowed => None,
        SkillGate::Trusted(digest) if *digest == set.digest => None,
        SkillGate::Trusted(digest) => Some(format!(
            "the repository's skills changed since they were trusted: set {} was trusted, set \
             {} is at commit {}",
            short(digest),
            short(&set.digest),
            short(&set.commit)
        )),
        SkillGate::Withheld(why) => Some(why.clone()),
    };
    Some(BranchSkills {
        entries: set.entries(),
        commit: set.commit,
        digest: set.digest,
        withheld,
    })
}

/// What the branch with `skills` stages this turn: its staged skills read
/// again from their commit in the repository at `root`, and withheld if
/// they cannot be read or no longer match their digest.
pub(crate) fn for_turn(root: &Path, skills: Option<&BranchSkills>) -> ForTurn {
    let Some(skills) = skills else {
        return ForTurn::Nothing;
    };
    let withheld = |reason: String| {
        ForTurn::Withheld(WithheldSkills {
            entries: skills.entries.clone(),
            reason,
        })
    };
    if let Some(reason) = &skills.withheld {
        return withheld(reason.clone());
    }
    match read(root, &skills.commit) {
        Ok(Found::Skills(set)) if set.digest == skills.digest => ForTurn::Staged(set),
        Ok(_) => withheld(format!(
            "the repository's skills at commit {} no longer match set {}",
            short(&skills.commit),
            short(&skills.digest)
        )),
        Err(error) => withheld(format!(
            "the repository's skills could not be read at commit {}: {error}",
            short(&skills.commit)
        )),
    }
}

/// The record of a turn that staged `built_in`, what `repository` says,
/// and the person's own `user` skills when the branch has a folder.
pub(crate) fn skill_set(
    built_in: Vec<SkillEntry>,
    repository: &ForTurn,
    user: Option<&UserSet>,
) -> SkillSet {
    let mut entries = built_in;
    let withheld = match repository {
        ForTurn::Nothing => None,
        ForTurn::Staged(set) => {
            entries.extend(set.entries());
            None
        }
        ForTurn::Withheld(withheld) => Some(withheld.clone()),
    };
    let user_withheld = match user {
        Some(user) => {
            entries.extend(user.entries());
            user.withheld.clone()
        }
        None => Vec::new(),
    };
    SkillSet {
        set: skillset::set_digest(&entries),
        entries,
        withheld,
        user_withheld,
    }
}

/// A person's skills folder: [`USER_SKILLS`] beside the user configuration
/// file `config` (`~/.config/branchyard/config.toml` gives
/// `~/.config/branchyard/skills`).
pub fn user_dir(config: &Path) -> PathBuf {
    config.with_file_name(USER_SKILLS)
}

/// Read the person's own skills under `dir`, their skills folder. A skill
/// is a directory there with a `SKILL.md`; a loose file, and a directory
/// without one, is skipped. A directory that is a symbolic link, holds
/// one, holds a file that is not UTF-8 text, or that the skill checks
/// refuse is withheld by name with the reason; a folder that does not
/// exist holds nothing, and one that cannot be read withholds everything
/// under the name `*`.
pub fn read_user(dir: &Path) -> UserSet {
    let mut set = UserSet::default();
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return set,
        Err(error) => {
            set.withheld.push(WithheldSkill {
                name: "*".into(),
                reason: format!("{} could not be read: {error}", dir.display()),
            });
            return set;
        }
    };
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for entry in listing.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        names.push((name, entry.path()));
    }
    names.sort();
    for (name, path) in names {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            set.withheld.push(WithheldSkill {
                name,
                reason: "is a symbolic link; skills are read only from the folder's own \
                         directories"
                    .into(),
            });
            continue;
        }
        if !meta.is_dir() || !path.join("SKILL.md").is_file() {
            continue;
        }
        match read_directory(&name, &path) {
            Ok(skill) => set.skills.push(skill),
            Err(reason) => set.withheld.push(WithheldSkill { name, reason }),
        }
    }
    set
}

/// The files of the skill directory `dir`, named `name`, checked as a
/// skill: no symbolic link, UTF-8 text only, within the limits, a
/// `SKILL.md` whose frontmatter names `name`.
pub fn read_directory(name: &str, dir: &Path) -> Result<Skill, String> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let here = dir.join(&relative);
        let listing = std::fs::read_dir(&here)
            .map_err(|e| format!("{} could not be read: {e}", here.display()))?;
        for entry in listing {
            let entry = entry.map_err(|e| format!("{} could not be read: {e}", here.display()))?;
            let path = relative.join(entry.file_name());
            let shown = path.display().to_string();
            let meta = std::fs::symlink_metadata(entry.path())
                .map_err(|e| format!("{shown} could not be read: {e}"))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{shown} is a symbolic link, which a skill may not hold"
                ));
            }
            if meta.is_dir() {
                pending.push(path);
                continue;
            }
            if meta.len() > skillset::MAX_SKILL_BYTES {
                return Err(format!(
                    "skill {name}: {shown} holds {} bytes; a skill holds at most {} (1 MiB)",
                    meta.len(),
                    skillset::MAX_SKILL_BYTES
                ));
            }
            let bytes = std::fs::read(entry.path())
                .map_err(|e| format!("{shown} could not be read: {e}"))?;
            let text = String::from_utf8(bytes).map_err(|_| {
                format!("skill {name}: {shown} is not UTF-8 text; skills hold text files only")
            })?;
            files.push((shown, text));
            if files.len() > skillset::MAX_SKILL_FILES {
                return Err(format!(
                    "skill {name} has more than {} files",
                    skillset::MAX_SKILL_FILES
                ));
            }
        }
    }
    Skill::new(name, files)
}

/// The person's own skills for one turn: [`read_user`] over `dir`, then
/// the precedence rule ([`Source::precedence`]): a skill with a built-in's
/// name is withheld (reserved), and so is one the repository's staged set
/// names, as shadowed by `repository:<name>`.
pub(crate) fn user_for_turn(dir: &Path, repository: &ForTurn) -> UserSet {
    let mut set = read_user(dir);
    let staged: Vec<&str> = match repository {
        ForTurn::Staged(set) => set.skills.iter().map(|s| s.name.as_str()).collect(),
        ForTurn::Nothing | ForTurn::Withheld(_) => Vec::new(),
    };
    let mut kept = Vec::new();
    for skill in std::mem::take(&mut set.skills) {
        if skillset::reserved(&skill.name) {
            set.withheld.push(WithheldSkill {
                reason: format!(
                    "{} is a built-in skill's name, which is reserved",
                    skill.name
                ),
                name: skill.name,
            });
        } else if staged.contains(&skill.name.as_str()) {
            set.withheld.push(WithheldSkill {
                reason: format!(
                    "shadowed by {}: a repository's skill comes before yours",
                    skillset::label(Source::Repository, &skill.name)
                ),
                name: skill.name,
            });
        } else {
            kept.push(skill);
        }
    }
    set.skills = kept;
    set.withheld.sort_by(|a, b| a.name.cmp(&b.name));
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = branchyard_support::child::command("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "t"]);
        dir
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn commit(root: &Path) -> String {
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "--allow-empty", "-m", "c"]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn skill(root: &Path, name: &str, description: &str) {
        skill_in_repo(root, name, description);
    }

    fn skill_in_repo(root: &Path, name: &str, description: &str) {
        write(
            root,
            &format!(".agents/skills/{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
        );
    }

    fn found(root: &Path, rev: &str) -> RepositorySet {
        match read(root, rev).unwrap() {
            Found::Skills(set) => set,
            other => panic!("{other:?}"),
        }
    }

    fn refused(root: &Path) -> String {
        match read(root, "HEAD").unwrap() {
            Found::Refused(why) => why,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn skills_are_read_from_the_commit_not_the_worktree() {
        let dir = repo();
        let root = dir.path();
        commit(root);
        assert_eq!(read(root, "HEAD").unwrap(), Found::Nothing);
        skill(root, "style-guide", "House style.");
        write(
            root,
            ".agents/skills/style-guide/references/terms.md",
            "terms",
        );
        write(root, ".agents/skills/README.md", "not a skill");
        write(
            root,
            ".agents/skills/shared/notes.md",
            "no SKILL.md: not a skill",
        );
        let first = commit(root);
        skill(root, "style-guide", "Edited in the worktree only.");
        let set = found(root, "HEAD");
        assert_eq!(set.commit, first);
        assert_eq!(set.skills.len(), 1);
        assert_eq!(set.skills[0].description, "House style.");
        assert_eq!(
            set.skills[0]
                .files
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            ["SKILL.md", "references/terms.md"]
        );
        assert_eq!(set.digest, skillset::set_digest(&set.entries()));
        let second = commit(root);
        let changed = found(root, &second);
        assert_ne!(changed.digest, set.digest);
        assert_eq!(
            found(root, &first).digest,
            set.digest,
            "a commit reads the same"
        );
        assert!(matches!(read(root, "no-such-rev"), Err(Error::Git(_))));
    }

    #[test]
    fn links_submodule_like_entries_bad_text_and_bad_skills_refuse_the_set() {
        let dir = repo();
        let root = dir.path();
        skill(root, "a", "A.");
        std::os::unix::fs::symlink("SKILL.md", root.join(".agents/skills/a/link.md")).unwrap();
        commit(root);
        assert!(refused(root).contains("skill a: link.md is a symbolic link"));
        std::fs::remove_file(root.join(".agents/skills/a/link.md")).unwrap();
        std::fs::write(root.join(".agents/skills/a/bin"), [0xff, 0xfe]).unwrap();
        commit(root);
        assert!(refused(root).contains("skill a: bin is not UTF-8 text"));
        std::fs::remove_file(root.join(".agents/skills/a/bin")).unwrap();
        write(
            root,
            ".agents/skills/a/SKILL.md",
            "---\nname: b\ndescription: d\n---\n",
        );
        commit(root);
        assert!(refused(root).contains("the name must be its directory's"));
        skill(root, "a", "A.");
        skill(root, "docx", "Mine.");
        commit(root);
        assert!(refused(root).contains("repository skill docx has a built-in skill's name"));
    }

    #[test]
    fn a_linked_skills_directory_is_refused() {
        let dir = repo();
        let root = dir.path();
        skill(root, "a", "A.");
        std::fs::rename(root.join(".agents/skills"), root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", root.join(".agents/skills")).unwrap();
        commit(root);
        assert!(refused(root).contains(".agents/skills is a symbolic link"));
    }

    #[test]
    fn too_many_skills_are_refused_before_their_files_are_read() {
        let dir = repo();
        let root = dir.path();
        for i in 0..120 {
            skill(root, &format!("s{i}"), "d");
        }
        commit(root);
        assert!(refused(root).contains("120 skills cannot fit"));
    }

    #[test]
    fn the_gate_decides_what_a_new_branch_stages_and_a_turn_reads_it_again() {
        let dir = repo();
        let root = dir.path();
        skill(root, "a", "A.");
        let base = commit(root);
        let set = found(root, &base);
        let staged = fix(root, &base, &SkillGate::Allowed).unwrap();
        assert_eq!(staged.withheld, None);
        assert_eq!(staged.entries, set.entries());
        assert_eq!(
            fix(root, &base, &SkillGate::Trusted(set.digest.clone())).unwrap(),
            staged
        );
        let changed = fix(root, &base, &SkillGate::Trusted("0".repeat(64))).unwrap();
        assert!(changed
            .withheld
            .unwrap()
            .contains("changed since they were trusted"));
        let default = fix(root, &base, &SkillGate::default()).unwrap();
        assert_eq!(default.withheld.as_deref(), Some(NOT_ALLOWED));
        assert_eq!(default.entries, set.entries(), "named even when withheld");

        // The worktree moves on; the branch's skills do not.
        skill(root, "a", "Changed.");
        commit(root);
        assert_eq!(for_turn(root, Some(&staged)), ForTurn::Staged(set.clone()));
        let record = skill_set(Vec::new(), &for_turn(root, Some(&staged)), None);
        assert_eq!(record.entries, set.entries());
        assert_eq!(record.set, skillset::set_digest(&set.entries()));
        assert_eq!(record.withheld, None);
        match for_turn(root, Some(&default)) {
            ForTurn::Withheld(w) => assert_eq!(w.reason, NOT_ALLOWED),
            other => panic!("{other:?}"),
        }
        let tampered = BranchSkills {
            digest: "0".repeat(64),
            ..staged.clone()
        };
        match for_turn(root, Some(&tampered)) {
            ForTurn::Withheld(w) => assert!(w.reason.contains("no longer match")),
            other => panic!("{other:?}"),
        }
        assert_eq!(for_turn(root, None), ForTurn::Nothing);
        assert_eq!(fix(dir.path(), &base, &SkillGate::Allowed), Some(staged));
    }

    #[test]
    fn a_linked_or_non_utf8_skill_refuses_the_set_and_a_loose_one_is_skipped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = repo();
        let root = dir.path();
        skill(root, "a", "A.");
        let loose = std::ffi::OsStr::from_bytes(b".agents/skills/notes/caf\xe9.md");
        std::fs::create_dir_all(root.join(".agents/skills/notes")).unwrap();
        std::fs::write(root.join(loose), "not a skill").unwrap();
        commit(root);
        assert_eq!(
            found(root, "HEAD").skills.len(),
            1,
            "a loose path is skipped"
        );
        let inside = std::ffi::OsStr::from_bytes(b".agents/skills/a/caf\xe9.md");
        std::fs::write(root.join(inside), "in a skill").unwrap();
        commit(root);
        assert!(refused(root).contains("skill a: the path"));
        std::fs::remove_file(root.join(inside)).unwrap();
        std::os::unix::fs::symlink("a", root.join(".agents/skills/b")).unwrap();
        commit(root);
        assert!(refused(root).contains(".agents/skills/b is a symbolic link"));
    }

    #[test]
    fn replace_refs_do_not_change_what_a_commit_holds() {
        let dir = repo();
        let root = dir.path();
        skill(root, "a", "Original.");
        commit(root);
        let original = git(root, &["rev-parse", "HEAD:.agents/skills/a/SKILL.md"]);
        std::fs::write(
            root.join("other"),
            "---\nname: a\ndescription: Swapped.\n---\n",
        )
        .unwrap();
        let swapped = git(root, &["hash-object", "-w", "other"]);
        git(root, &["replace", &original, &swapped]);
        assert!(git(root, &["show", "HEAD:.agents/skills/a/SKILL.md"]).contains("Swapped"));
        assert_eq!(found(root, "HEAD").skills[0].description, "Original.");
    }

    #[test]
    fn skills_that_cannot_be_read_or_are_refused_never_stop_a_branch() {
        let dir = repo();
        let root = dir.path();
        skill(root, "docx", "Mine.");
        let base = commit(root);
        let withheld = fix(root, &base, &SkillGate::Withheld("not trusted".into())).unwrap();
        let reason = withheld.withheld.unwrap();
        assert!(
            reason.starts_with("not trusted; and the repository's skills at"),
            "{reason}"
        );
        assert!(reason.contains("built-in skill's name"), "{reason}");
        let allowed = fix(root, &base, &SkillGate::Allowed).unwrap();
        assert!(allowed
            .withheld
            .unwrap()
            .starts_with("the repository's skills at"));

        skill(root, "docx", "Changed.");
        std::fs::rename(
            root.join(".agents/skills/docx"),
            root.join(".agents/skills/mine"),
        )
        .unwrap();
        write(
            root,
            ".agents/skills/mine/SKILL.md",
            "---\nname: mine\ndescription: Mine.\n---\n",
        );
        let base = commit(root);
        let blob = git(root, &["rev-parse", "HEAD:.agents/skills/mine/SKILL.md"]);
        let object = root.join(".git/objects").join(&blob[..2]).join(&blob[2..]);
        std::fs::remove_file(object).unwrap();
        assert!(read(root, &base).is_err());
        let missing = fix(root, &base, &SkillGate::Allowed).unwrap();
        let reason = missing.withheld.unwrap();
        assert!(reason.contains("could not be read at commit"), "{reason}");
        assert!(missing.entries.is_empty());
    }

    #[test]
    fn a_persons_skills_are_read_from_their_folder_and_withheld_by_name() {
        let home = tempfile::tempdir().unwrap();
        let dir = user_dir(&home.path().join("config.toml"));
        assert_eq!(dir, home.path().join("skills"));
        assert_eq!(read_user(&dir), UserSet::default(), "no folder: nothing");
        let skill = |name: &str, description: &str| {
            write(
                &dir,
                &format!("{name}/SKILL.md"),
                &format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
            );
        };
        skill("memory-parser", "What you learned about the parser.");
        write(&dir, "memory-parser/references/bench.md", "# Bench\n");
        skill("docx", "Mine.");
        skill("style-guide", "Mine too.");
        write(&dir, "notes.md", "a loose file is not a skill");
        write(&dir, "empty/README.md", "no SKILL.md: not a skill");
        write(
            &dir,
            "bad/SKILL.md",
            "---\nname: other\ndescription: d\n---\n",
        );
        std::fs::write(dir.join("memory-parser/bin"), [0xff, 0xfe]).unwrap();
        let set = read_user(&dir);
        assert_eq!(
            set.skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["docx", "style-guide"]
        );
        assert_eq!(set.withheld.len(), 2, "{:?}", set.withheld);
        assert_eq!(set.withheld[0].name, "bad");
        assert!(set.withheld[0]
            .reason
            .contains("the name must be its directory's"));
        assert_eq!(set.withheld[1].name, "memory-parser");
        assert!(set.withheld[1].reason.contains("bin is not UTF-8 text"));

        std::fs::remove_file(dir.join("memory-parser/bin")).unwrap();
        std::os::unix::fs::symlink("SKILL.md", dir.join("style-guide/link.md")).unwrap();
        std::os::unix::fs::symlink("docx", dir.join("linked")).unwrap();
        let set = read_user(&dir);
        assert_eq!(
            set.skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["docx", "memory-parser"]
        );
        let reasons: Vec<(&str, &str)> = set
            .withheld
            .iter()
            .map(|w| (w.name.as_str(), w.reason.as_str()))
            .collect();
        assert_eq!(reasons[1].0, "linked");
        assert!(reasons[1]
            .1
            .contains("is a symbolic link; skills are read only"));
        assert_eq!(reasons[2].0, "style-guide");
        assert!(reasons[2].1.contains("link.md is a symbolic link"));
        assert_eq!(
            set.skills[1]
                .files
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            ["SKILL.md", "references/bench.md"]
        );
        assert_eq!(set.entries()[0].label(), "user:docx");

        // Precedence: a built-in's name is reserved, a staged repository
        // skill shadows a user skill of its name, and a withheld repository
        // set shadows nothing.
        std::fs::remove_file(dir.join("style-guide/link.md")).unwrap();
        let repo = repo();
        skill_in_repo(repo.path(), "style-guide", "House style.");
        let base = commit(repo.path());
        let staged = ForTurn::Staged(found(repo.path(), &base));
        let turn = user_for_turn(&dir, &staged);
        assert_eq!(
            turn.skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["memory-parser"]
        );
        let withheld: Vec<(&str, &str)> = turn
            .withheld
            .iter()
            .map(|w| (w.name.as_str(), w.reason.as_str()))
            .collect();
        assert_eq!(withheld[1].0, "docx");
        assert!(withheld[1]
            .1
            .contains("built-in skill's name, which is reserved"));
        assert_eq!(withheld[3].0, "style-guide");
        assert_eq!(
            withheld[3].1,
            "shadowed by repository:style-guide: a repository's skill comes before yours"
        );
        let unstaged = user_for_turn(&dir, &ForTurn::Nothing);
        assert_eq!(
            unstaged
                .skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["memory-parser", "style-guide"]
        );
        let record = skill_set(Vec::new(), &ForTurn::Nothing, Some(&unstaged));
        assert_eq!(
            record
                .entries
                .iter()
                .map(SkillEntry::label)
                .collect::<Vec<_>>(),
            ["user:memory-parser", "user:style-guide"]
        );
        assert_eq!(record.user_withheld.len(), 3);
        assert_eq!(record.set, skillset::set_digest(&record.entries));
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["entries"][0]["layer"], "user");
        assert_eq!(json["user_withheld"][0]["name"], "bad");
        let empty = skill_set(Vec::new(), &ForTurn::Nothing, Some(&UserSet::default()));
        assert!(!serde_json::to_string(&empty)
            .unwrap()
            .contains("user_withheld"));
        let mut unreadable = user_dir(&home.path().join("config.toml"));
        unreadable.push("notes.md");
        assert_eq!(read_user(&unreadable).withheld[0].name, "*");
    }

    #[test]
    fn identical_files_in_two_skills_are_both_read() {
        let dir = repo();
        let root = dir.path();
        skill(root, "a", "Same.");
        skill(root, "b", "Same.");
        write(root, ".agents/skills/a/x.md", "same");
        write(root, ".agents/skills/b/x.md", "same");
        commit(root);
        let set = found(root, "HEAD");
        assert_eq!(set.skills.len(), 2);
        assert!(set.skills.iter().all(|s| s.files.len() == 2));
    }
}
