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

use std::collections::BTreeMap;
use std::path::Path;

use branchyard_provision::skillset;
pub use branchyard_provision::skillset::{Layer, Skill, SkillEntry};
use branchyard_workspace::Git;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Where a repository keeps its skills, one directory each.
pub const REPOSITORY_SKILLS: &str = ".agents/skills";

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
    /// Each skill's entry, in the repository layer.
    pub fn entries(&self) -> Vec<SkillEntry> {
        self.skills
            .iter()
            .map(|skill| skill.entry(Layer::Repository))
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
    /// BLAKE3 over `entries`' layers, names and digests, in layer and name
    /// order.
    pub set: String,
    /// Every skill staged for the turn, built-in and custom.
    pub entries: Vec<SkillEntry>,
    /// The repository's skills that were found and not staged, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<WithheldSkills>,
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

/// The built-in skills every turn stages, each in the built-in layer, in
/// catalog order.
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
    if let Err(why) = skillset::custom_index(branchyard_provision::skills::INDEX, &checked, &commit)
    {
        return refused(why);
    }
    let digest = skillset::set_digest(
        &checked
            .iter()
            .map(|skill| skill.entry(Layer::Repository))
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

/// The record of a turn that staged `built_in` and what `repository` says.
pub(crate) fn skill_set(built_in: Vec<SkillEntry>, repository: &ForTurn) -> SkillSet {
    let mut entries = built_in;
    let withheld = match repository {
        ForTurn::Nothing => None,
        ForTurn::Staged(set) => {
            entries.extend(set.entries());
            None
        }
        ForTurn::Withheld(withheld) => Some(withheld.clone()),
    };
    SkillSet {
        set: skillset::set_digest(&entries),
        entries,
        withheld,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
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
        let record = skill_set(Vec::new(), &for_turn(root, Some(&staged)));
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
