//! The tool tree a turn is given: one directory, found by search.
//!
//! Before each turn the chosen skills and connectors are listed under
//! `.branchyard/tools/` of the branch's home, or of its cache when it has no
//! private home:
//!
//! ```text
//! .branchyard/tools/
//!   INDEX.md      at most 4 KB: one `- name: description` line per tool, how to search
//!   bin/tools     the find-tools skill's script: `search`, `show`, `schema`
//!   skills        link to ../capabilities, where the skills are staged
//!   connectors    link to ../connectors, where Anvil's packages are placed
//!   helpers/INDEX.md  only for a turn that may delegate: the envelope, one
//!                 line per delegate-skill reference, one per harness it may
//!                 hand work to
//!   helpers/delegate/references/NAME.md  those references, which `tools
//!                 search` finds and the delegate skill's table names
//! ```
//!
//! Nothing is copied twice: the links point at the directories
//! [`crate::capabilities`] and [`crate::connectors`] already write, and those
//! paths keep working. The harness is given one line ([`Mounted::line`])
//! instead of a line for the skills and a line for the connectors. The
//! tree is rewritten each turn, so it is the turn's set whatever an earlier
//! turn left. `INDEX.md` names the *chosen* skills and counts the rest in
//! its overflow line (`- ...N more: bin/tools search WORDS`): the skills
//! that are staged and findable but were not picked for the turn, where
//! every ready skill is mounted (`docs/skill-registry.md`, R2), and any
//! chosen skill the budget had no room to name. Where the provisioner owns
//! a private home for Claude Code or Codex, the chosen skills, and only
//! those, are also linked into the harness's own skills directory
//! ([`link_native`]), so a harness that loads every frontmatter there loads
//! only what was chosen. See `docs/knowledge-skills.md`.

use std::fs;
use std::io;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use branchyard_provision::{apply, skills, Edit, Plan};

/// The tree, relative to the home.
pub(crate) const DIRECTORY: &str = ".branchyard/tools";
/// The most bytes `INDEX.md` takes.
pub(crate) const INDEX_BUDGET: usize = 4096;
/// The most bytes `helpers/INDEX.md` takes.
pub(crate) const HELPERS_BUDGET: usize = 2048;
/// Where the delegate skill's reference files sit below the tree, for a
/// turn with helpers: `helpers/delegate/references/NAME.md`.
const DELEGATE_REFERENCES: &str = "helpers/delegate/references";
/// The most characters of the one line given to the harness.
pub(crate) const LINE_BUDGET: usize = 300;
/// The skill that carries the script, and the script's path in it.
const FIND_TOOLS: &str = "find-tools";
const SCRIPT: &str = "scripts/tools.py";
/// Where skills are staged, relative to the home.
const CAPABILITIES: &str = crate::capabilities::DIRECTORY;
/// Where connector packages are placed, relative to the home.
const CONNECTORS: &str = crate::connectors::HOME_DIR;
/// The composite's directory under [`CONNECTORS`].
const COMPOSE: &str = "_compose";

/// A skill or connector listed in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tool {
    /// What the index and `tools show` call it.
    pub id: String,
    /// One sentence, from its frontmatter or the connector's index.
    pub description: String,
    /// Its directory below `.branchyard/capabilities/`, for a skill.
    pub directory: String,
}

/// What mounting the tree gave the turn.
#[derive(Debug)]
pub(crate) struct Mounted {
    /// The one line for the harness's prompt.
    pub line: String,
    /// Files and links written or removed, relative to the home.
    pub written: Vec<String>,
}

/// Where a guest whose filesystem a provider controls also sees the tree.
pub(crate) const GUEST_ROOT: &str = "/tools";
/// The variable that names the tree to a harness and to `bin/tools`: the
/// root a provider that controls the guest filesystem also exposes it at.
pub(crate) const ENV_TOOLS: &str = "BRANCHYARD_TOOLS";

/// Mount the tree in `host`, naming paths as the guest sees them under
/// `guest`: `chosen` are the skills the index names (the built-in skills
/// the task picked), `mounted` those staged beside them that it only
/// counts (the rest of the allowed built-ins where every ready skill is
/// mounted, the repository's and the person's), `connectors` the ids of
/// the turn's grant, and `helpers` the turn's delegation envelope. A turn
/// whose envelope has no helpers (a leaf) gets no `helpers/INDEX.md`, and
/// one an earlier turn wrote is removed.
///
/// With `root` (`/tools`), the guest also sees the tree there, as a read-only
/// mount or a copy a provider makes, and not only under the home. A relative
/// link would then point out of the tree to the wrong place, so its two links
/// name their targets by absolute guest path, and the line names `root`.
pub(crate) fn mount(
    host: &Path,
    guest: &Path,
    chosen: &[Tool],
    mounted: &[Tool],
    connectors: &[String],
    helpers: Option<&crate::Envelope>,
    root: Option<&str>,
) -> Result<Mounted, String> {
    let helpers = helpers.filter(|e| e.max_depth > 0 && e.max_children > 0);
    let described = connector_tools(host, connectors)?;
    let script = skills::skill(FIND_TOOLS)
        .and_then(|skill| skill.files.iter().find(|(path, _)| *path == SCRIPT))
        .map(|(_, content)| *content)
        .ok_or("the find-tools skill has no script")?;
    let mut plan = Plan::default();
    plan.edit(
        &format!("{DIRECTORY}/INDEX.md"),
        false,
        Edit::Put(render_index(
            chosen,
            mounted.len(),
            &described,
            helpers.is_some(),
        )),
    );
    plan.edit(
        &format!("{DIRECTORY}/helpers/INDEX.md"),
        false,
        match helpers {
            Some(envelope) => Edit::Put(render_helpers(envelope)),
            None => Edit::Remove,
        },
    );
    for (name, text) in delegate_references() {
        plan.edit(
            &format!("{DIRECTORY}/{DELEGATE_REFERENCES}/{name}.md"),
            false,
            match helpers {
                Some(_) => Edit::Put(text.into()),
                None => Edit::Remove,
            },
        );
    }
    let script_path = format!("{DIRECTORY}/bin/tools");
    plan.edit(&script_path, false, Edit::Put(script.into()));
    let applied =
        apply::apply(&plan, host).map_err(|e| format!("could not mount the tool tree: {e}"))?;
    let mut written: Vec<String> = applied.written.into_iter().chain(applied.removed).collect();
    let fail = |e: io::Error| format!("could not mount the tool tree: {e}");
    fs::set_permissions(host.join(&script_path), fs::Permissions::from_mode(0o755))
        .map_err(fail)?;
    let tree = host.join(DIRECTORY);
    let (skills_to, connectors_to) = match root {
        None => ("../capabilities".to_owned(), "../connectors".to_owned()),
        Some(_) => (
            guest.join(CAPABILITIES).display().to_string(),
            guest.join(CONNECTORS).display().to_string(),
        ),
    };
    for (name, target, wanted) in [
        ("skills", skills_to.as_str(), true),
        ("connectors", connectors_to.as_str(), !connectors.is_empty()),
    ] {
        if relink(&tree.join(name), target, wanted).map_err(fail)? {
            written.push(format!("{DIRECTORY}/{name}"));
        }
    }
    let directory = match root {
        Some(root) => root.trim_end_matches('/').to_owned(),
        None => guest.join(DIRECTORY).display().to_string(),
    };
    Ok(Mounted {
        line: one_line(&directory),
        written,
    })
}

/// The one line the harness is given. It names the index and the search,
/// and stays under [`LINE_BUDGET`] characters whatever the home is called.
fn one_line(directory: &str) -> String {
    let long = format!(
        "Your tools: read {directory}/INDEX.md. Find one with `{directory}/bin/tools search WORDS` \
         before you open a skill."
    );
    let short = format!(
        "Your tools: read {directory}/INDEX.md. Find one with `bin/tools search WORDS` run in that \
         directory, before you open a skill."
    );
    for line in [long, short] {
        if line.chars().count() <= LINE_BUDGET {
            return line;
        }
    }
    "Your tools: read INDEX.md in .branchyard/tools of your home, and run bin/tools search WORDS \
     there before you open a skill."
        .into()
}

/// Make `path` a link to `target` when `wanted`, and remove it otherwise;
/// only a link is replaced or removed. Whether anything changed.
fn relink(path: &Path, target: &str, wanted: bool) -> io::Result<bool> {
    let mut changed = false;
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if wanted && fs::read_link(path)?.as_os_str() == target {
                return Ok(false);
            }
            fs::remove_file(path)?;
            changed = true;
        }
        // The harness put something of its own here; leave it.
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if wanted {
        symlink(target, path)?;
        changed = true;
    }
    Ok(changed)
}

/// The connectors of the grant, each with the description its package gives
/// (its `SKILL.md` frontmatter), and the composite when it was written.
fn connector_tools(host: &Path, ids: &[String]) -> Result<Vec<Tool>, String> {
    let base = host.join(CONNECTORS);
    let mut out = Vec::new();
    for id in ids {
        let path = base.join(id).join("SKILL.md");
        // A package without a SKILL.md is listed bare; any other failure
        // to read one is the turn's.
        let described = match fs::read_to_string(&path) {
            Ok(text) => match branchyard_provision::skillset::frontmatter(&text) {
                Ok(front) => front.description,
                Err(_) => String::new(),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        out.push(Tool {
            id: id.clone(),
            description: match described.trim().is_empty() {
                true => "a connector; read its SKILL.md".into(),
                false => described.split_whitespace().collect::<Vec<_>>().join(" "),
            },
            directory: String::new(),
        });
    }
    if base.join(COMPOSE).join("SKILL.md").is_file() {
        out.push(Tool {
            id: COMPOSE.into(),
            description: "one client and flows across the connectors above".into(),
            directory: String::new(),
        });
    }
    Ok(out)
}

/// What a connector's calls need beyond its package, said once for all.
const CONNECTOR_RULES: &str = "Call connectors only through the gateway; never ask for or use \
    upstream credentials. When a call carries out a decision you might repeat after a restart \
    (a payment, a message), name it: `_meta.decision_key` on the MCP call, or the \
    `X-Branchyard-Decision-Key` header, such as `invoice-7-payment`; a repeat of a decision \
    already made is refused with what is known.";

/// `INDEX.md`: how to search, then a line for each chosen tool and one
/// counting the `mounted` skills it does not name, with descriptions cut
/// as far as it takes to fit [`INDEX_BUDGET`].
fn render_index(skills: &[Tool], mounted: usize, connectors: &[Tool], helpers: bool) -> String {
    for cap in [160, 120, 90, 60, 40, 0] {
        let text = index_with(skills, mounted, connectors, helpers, cap, usize::MAX);
        if text.len() <= INDEX_BUDGET {
            return text;
        }
    }
    // Names alone, as many as fit.
    let mut shown = skills.len() + connectors.len();
    loop {
        let text = index_with(skills, mounted, connectors, helpers, 0, shown);
        if text.len() <= INDEX_BUDGET || shown == 0 {
            return text;
        }
        shown -= 1;
    }
}

fn index_with(
    skills: &[Tool],
    mounted: usize,
    connectors: &[Tool],
    helpers: bool,
    cap: usize,
    shown: usize,
) -> String {
    let mut text = String::from(
        "# Your tools\n\nSearch first: `bin/tools search WORDS` ranks everything below; \
         `bin/tools show ID` prints one; `bin/tools schema CONNECTOR.OP` prints an operation's \
         input schema. Run them from this directory. Open a skill's SKILL.md only after a search \
         names it.\n",
    );
    let mut left = shown;
    // `extra` are tools in the tree the section never names: the mounted
    // skills, counted with any chosen one the budget cut.
    let mut section = |title: &str, tools: &[Tool], extra: usize, text: &mut String| {
        if tools.is_empty() && extra == 0 {
            return;
        }
        text.push_str(&format!("\n## {title}\n"));
        for tool in tools.iter().take(left) {
            text.push_str(&format!(
                "- {}{}\n",
                tool.id,
                describe(&tool.description, cap)
            ));
        }
        let counted = tools.len().saturating_sub(left) + extra;
        if counted > 0 {
            text.push_str(&format!("- ...{counted} more: `bin/tools search WORDS`\n"));
        }
        left = left.saturating_sub(tools.len());
    };
    section("Skills (skills/NAME/SKILL.md)", skills, mounted, &mut text);
    section(
        "Connectors (connectors/ID/SKILL.md)",
        connectors,
        0,
        &mut text,
    );
    if helpers {
        text.push_str(
            "\n## Helpers\nYou may hand work to other harnesses: `helpers/INDEX.md` lists them and \
             your limits; `bin/tools search --kind helper WORDS` finds one.\n",
        );
    }
    if !connectors.is_empty() {
        text.push('\n');
        text.push_str(CONNECTOR_RULES);
        text.push('\n');
    }
    text
}

/// The delegate skill's reference files, as `(name without .md, text)`: what
/// a turn that may delegate reads on demand instead of being given them.
fn delegate_references() -> Vec<(&'static str, &'static str)> {
    crate::projection::references()
        .into_iter()
        .filter_map(|(path, text)| {
            let name = path.strip_prefix("references/")?.strip_suffix(".md")?;
            Some((name, text))
        })
        .collect()
}

/// `helpers/INDEX.md`: the envelope's limits, then one line for each of the
/// pointer to the delegate skill's reference files, then one
/// `- id: description` line for each harness it lets the turn delegate to. The catalog records
/// a harness's name and models, not what it is best at, so a line says what
/// it is and which models it takes. Small by construction: lines past
/// [`HELPERS_BUDGET`] are replaced by a count.
fn render_helpers(envelope: &crate::Envelope) -> String {
    let mut text = String::from("# Helpers\n\n");
    text.push_str(&format!(
        "You may have at most {} {} at once, {} {} deep. Hand one a task with the delegate \
         tools; it runs in its own branch.\n\n",
        envelope.max_children,
        if envelope.max_children == 1 {
            "helper"
        } else {
            "helpers"
        },
        envelope.max_depth,
        if envelope.max_depth == 1 {
            "level"
        } else {
            "levels"
        },
    ));
    text.push_str(&format!(
        "The delegate skill's details are files in this tree: `{DELEGATE_REFERENCES}/NAME.md`, \
         named in the skill's table.\n\n"
    ));
    if envelope.harnesses.is_empty() {
        text.push_str("Helpers run your own harness; no other is allowed.\n");
        return text;
    }
    for (n, id) in envelope.harnesses.iter().enumerate() {
        let line = format!("- {}\n", helper_line(id));
        if text.len() + line.len() + 80 > HELPERS_BUDGET {
            text.push_str(&format!(
                "- {} more: `bin/tools search --kind helper WORDS`\n",
                envelope.harnesses.len() - n
            ));
            break;
        }
        text.push_str(&line);
    }
    text
}

/// `id: Name, models a, b, c` for a harness or profile id the catalog
/// knows, else the id with a plain note.
fn helper_line(id: &str) -> String {
    let entry = crate::inventory::harness_of(id).and_then(branchyard_controls::catalog::harness);
    let Some(entry) = entry else {
        return format!("{id}: a harness this server runs");
    };
    let models: Vec<&str> = entry.models.iter().take(3).map(String::as_str).collect();
    match models.is_empty() {
        true => format!("{id}: {}", entry.name),
        false => format!("{id}: {}, models {}", entry.name, models.join(", ")),
    }
}

/// `: description` cut to `cap` characters, or nothing when `cap` is 0.
fn describe(description: &str, cap: usize) -> String {
    if cap == 0 || description.is_empty() {
        return String::new();
    }
    let flat: String = description.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.chars().count() <= cap {
        true => format!(": {flat}"),
        false => format!(
            ": {}...",
            flat.chars()
                .take(cap.saturating_sub(3))
                .collect::<String>()
                .trim_end()
        ),
    }
}

/// The harness's own skills directory in a private home, for the
/// harnesses that have one.
fn native_directory(harness: &str) -> Option<&'static str> {
    match harness {
        "claude-code" => Some(".claude/skills"),
        "codex" => Some(".codex/skills"),
        _ => None,
    }
}

/// Where the staged skills are, relative to a native skills directory two
/// levels below the home.
const STAGED_FROM_NATIVE: &str = "../../.branchyard/capabilities";

/// Link `skills`, the turn's *chosen* skills, into `harness`'s own skills
/// directory of the private home `home`, and remove the links an earlier
/// turn made to skills this turn does not choose. A merely mounted skill is
/// never linked here: the harness reads every frontmatter in this
/// directory into its own context with no budget, which is the cost the
/// tool tree exists to remove (`docs/skill-registry.md`, section 5). Only
/// links into the staged skills are touched: a skill the harness or its
/// user put there stays. Returns the links changed, relative to the home;
/// none for a harness without a skills directory, or when `.claude` or the
/// skills directory is itself a link.
pub(crate) fn link_native(
    home: &Path,
    harness: &str,
    skills: &[Tool],
) -> Result<Vec<String>, String> {
    let Some(native) = native_directory(harness) else {
        return Ok(Vec::new());
    };
    let fail = |e: io::Error| format!("could not link skills for {harness}: {e}");
    let mut dir = home.to_path_buf();
    for part in native.split('/') {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&dir).map_err(fail)?;
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(fail)?;
            }
            Err(e) => return Err(fail(e)),
        }
    }
    let ours = |path: &Path| {
        fs::read_link(path)
            .map(|target| target.starts_with(STAGED_FROM_NATIVE))
            .unwrap_or(false)
    };
    let mut changed = Vec::new();
    for entry in fs::read_dir(&dir).map_err(fail)? {
        let path = entry.map_err(fail)?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if ours(&path) && !skills.iter().any(|s| skill_name(s) == name) {
            fs::remove_file(&path).map_err(fail)?;
            changed.push(format!("{native}/{name}"));
        }
    }
    for skill in skills {
        let name = skill_name(skill);
        let target = format!("{STAGED_FROM_NATIVE}/{}", skill.directory);
        let path = dir.join(name);
        let exists = fs::symlink_metadata(&path).is_ok();
        if exists && !ours(&path) {
            continue;
        }
        if relink(&path, &target, true).map_err(fail)? {
            changed.push(format!("{native}/{name}"));
        }
    }
    changed.sort();
    Ok(changed)
}

/// A skill's name for its native link: the last part of its directory.
fn skill_name(skill: &Tool) -> &str {
    skill
        .directory
        .rsplit('/')
        .next()
        .unwrap_or(&skill.directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str, description: &str, directory: &str) -> Tool {
        Tool {
            id: id.into(),
            description: description.into(),
            directory: directory.into(),
        }
    }

    fn staged(host: &Path) -> Vec<Tool> {
        crate::capabilities::stage(host, None, None, None)
            .unwrap()
            .listed()
    }

    #[test]
    fn the_line_names_the_index_and_the_search_in_under_300_characters() {
        let line = one_line("/branchyard/home/.branchyard/tools");
        assert!(line.contains("/branchyard/home/.branchyard/tools/INDEX.md"));
        assert!(line.contains("bin/tools search WORDS"));
        assert!(line.chars().count() < LINE_BUDGET, "{line}");
        let deep = format!("/{}", "deep/".repeat(40));
        let fallback = one_line(&deep);
        assert!(fallback.chars().count() < LINE_BUDGET, "{fallback}");
        assert!(fallback.contains("bin/tools search"));
    }

    #[test]
    fn the_index_lists_every_tool_in_name_description_form_within_its_budget() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        assert!(skills.iter().any(|s| s.id == "find-tools"));
        let text = render_index(&skills, 0, &[], false);
        assert!(text.len() <= INDEX_BUDGET, "{}", text.len());
        for skill in &skills {
            assert!(
                text.lines()
                    .any(|l| l.starts_with(&format!("- {}", skill.id))),
                "{} is not listed",
                skill.id
            );
        }
        assert!(text.contains("- docx: Create, inspect"));
        assert!(
            !text.contains("decision_key"),
            "no connectors, no connector rules"
        );
    }

    #[test]
    fn a_long_list_loses_descriptions_then_names_and_still_fits() {
        let many: Vec<Tool> = (0..600)
            .map(|n| tool(&format!("skill-{n:03}"), &"word ".repeat(60), "x"))
            .collect();
        let text = render_index(&many, 0, &[], false);
        assert!(text.len() <= INDEX_BUDGET, "{}", text.len());
        assert!(text.contains("more: `bin/tools search WORDS`"), "{text}");
        let few: Vec<Tool> = (0..20)
            .map(|n| tool(&format!("skill-{n}"), &"word ".repeat(60), "x"))
            .collect();
        let text = render_index(&few, 0, &[], false);
        assert!(text.len() <= INDEX_BUDGET);
        assert!(text.contains("skill-19"), "{text}");
        assert!(
            text.contains("..."),
            "descriptions are cut with an ellipsis"
        );
    }

    #[test]
    fn mounting_writes_the_index_the_script_and_two_links_and_repeats_unchanged() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        let first = mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        let tree = host.path().join(DIRECTORY);
        assert!(first.line.contains("/g/.branchyard/tools/INDEX.md"));
        assert!(std::fs::read_to_string(tree.join("INDEX.md"))
            .unwrap()
            .starts_with("# Your tools"));
        let script = tree.join("bin/tools");
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
            0o111
        );
        assert_eq!(
            std::fs::read_link(tree.join("skills")).unwrap(),
            Path::new("../capabilities")
        );
        assert!(tree.join("skills/find-tools/SKILL.md").is_file());
        assert!(!tree.join("connectors").exists(), "no grant, no link");
        assert!(first.written.iter().any(|p| p.ends_with("bin/tools")));
        let again = mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        assert!(again.written.is_empty(), "{:?}", again.written);
    }

    #[test]
    fn a_tree_exposed_at_a_guest_root_links_by_absolute_guest_path_and_names_the_root() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        let granted = vec!["github".to_owned()];
        let mounted = mount(
            host.path(),
            Path::new("/branchyard/home"),
            &skills,
            &[],
            &granted,
            None,
            Some("/tools"),
        )
        .unwrap();
        // Read from /tools the links must not climb out of it.
        let tree = host.path().join(DIRECTORY);
        assert_eq!(
            std::fs::read_link(tree.join("skills")).unwrap(),
            Path::new("/branchyard/home/.branchyard/capabilities")
        );
        assert_eq!(
            std::fs::read_link(tree.join("connectors")).unwrap(),
            Path::new("/branchyard/home/.branchyard/connectors")
        );
        assert!(
            mounted.line.contains("read /tools/INDEX.md"),
            "{}",
            mounted.line
        );
        assert!(mounted.line.contains("`/tools/bin/tools search WORDS`"));
        assert!(mounted.line.chars().count() < LINE_BUDGET);
        // Remounted for a local turn, the links are relative again.
        mount(
            host.path(),
            Path::new("/g"),
            &skills,
            &[],
            &granted,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_link(tree.join("skills")).unwrap(),
            Path::new("../capabilities")
        );
    }

    #[test]
    fn the_script_runs_from_the_mounted_tree_and_finds_a_staged_skill() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        let output = branchyard_support::child::command("python3")
            .arg(host.path().join(DIRECTORY).join("bin/tools"))
            .args(["search", "arithmetic", "units"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(said.lines().next().unwrap().contains("calculate"), "{said}");
    }

    #[test]
    fn connectors_are_listed_linked_and_unlinked_when_the_grant_goes() {
        let host = tempfile::tempdir().unwrap();
        let package = host.path().join(CONNECTORS).join("github");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("SKILL.md"),
            "---\nname: github\ndescription: Issues and pull requests.\n---\n# GitHub\n",
        )
        .unwrap();
        let skills = staged(host.path());
        let granted = vec!["github".to_owned()];
        let first = mount(
            host.path(),
            Path::new("/g"),
            &skills,
            &[],
            &granted,
            None,
            None,
        )
        .unwrap();
        let tree = host.path().join(DIRECTORY);
        let index = std::fs::read_to_string(tree.join("INDEX.md")).unwrap();
        assert!(
            index.contains("- github: Issues and pull requests."),
            "{index}"
        );
        assert!(index.contains("`X-Branchyard-Decision-Key`"));
        assert!(index.len() <= INDEX_BUDGET);
        assert!(tree.join("connectors/github/SKILL.md").is_file());
        assert!(first.written.contains(&format!("{DIRECTORY}/connectors")));
        let none = mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        assert!(!tree.join("connectors").exists());
        assert!(none.written.contains(&format!("{DIRECTORY}/connectors")));
        let index = std::fs::read_to_string(tree.join("INDEX.md")).unwrap();
        assert!(!index.contains("github"));
    }

    #[test]
    fn a_directory_the_harness_put_where_a_link_goes_is_left_alone() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        let own = host.path().join(DIRECTORY).join("skills");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("mine.md"), "x").unwrap();
        mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        assert!(own.join("mine.md").is_file());
    }

    #[test]
    fn a_linked_tree_directory_is_refused_not_followed() {
        let host = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host.path().join(".branchyard")).unwrap();
        std::os::unix::fs::symlink(outside.path(), host.path().join(DIRECTORY)).unwrap();
        let skills = staged(host.path());
        assert!(mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn claude_and_codex_get_native_links_and_other_harnesses_none() {
        let home = tempfile::tempdir().unwrap();
        let skills = staged(home.path());
        assert!(link_native(home.path(), "gemini", &skills)
            .unwrap()
            .is_empty());
        assert!(!home.path().join(".claude").exists());
        let changed = link_native(home.path(), "claude-code", &skills).unwrap();
        assert_eq!(changed.len(), skills.len());
        let link = home.path().join(".claude/skills/docx");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../../.branchyard/capabilities/docx")
        );
        assert!(link.join("SKILL.md").is_file(), "the link resolves");
        assert!(link_native(home.path(), "claude-code", &skills)
            .unwrap()
            .is_empty());
        let codex = link_native(home.path(), "codex", &skills).unwrap();
        assert!(codex[0].starts_with(".codex/skills/"));
    }

    #[test]
    fn a_native_link_to_a_skill_the_turn_lost_goes_and_a_skill_of_the_users_stays() {
        let home = tempfile::tempdir().unwrap();
        let skills = staged(home.path());
        link_native(home.path(), "claude-code", &skills).unwrap();
        let native = home.path().join(".claude/skills");
        std::fs::create_dir_all(native.join("theirs")).unwrap();
        std::fs::write(native.join("theirs/SKILL.md"), "mine").unwrap();
        // A skill of theirs under a name the turn also stages is not replaced.
        std::fs::remove_file(native.join("calculate")).unwrap();
        std::fs::create_dir(native.join("calculate")).unwrap();
        let fewer: Vec<Tool> = skills.into_iter().filter(|s| s.id != "docx").collect();
        let changed = link_native(home.path(), "claude-code", &fewer).unwrap();
        assert_eq!(changed, [".claude/skills/docx"]);
        assert!(!native.join("docx").exists());
        assert!(native.join("theirs/SKILL.md").is_file());
        assert!(native.join("calculate").is_dir() && !native.join("calculate").is_symlink());
    }

    #[test]
    fn a_linked_native_directory_is_not_written_through() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), home.path().join(".claude")).unwrap();
        let skills = staged(home.path());
        assert!(link_native(home.path(), "claude-code", &skills)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn repository_skills_link_under_their_own_name() {
        let home = tempfile::tempdir().unwrap();
        let skills = vec![tool("repository:style", "House style.", "repository/style")];
        link_native(home.path(), "claude-code", &skills).unwrap();
        assert_eq!(
            std::fs::read_link(home.path().join(".claude/skills/style")).unwrap(),
            Path::new("../../.branchyard/capabilities/repository/style")
        );
    }

    fn envelope(harnesses: &[&str], max_children: u32, max_depth: u32) -> crate::Envelope {
        crate::Envelope {
            max_depth,
            max_children,
            harnesses: harnesses.iter().map(|h| (*h).to_owned()).collect(),
            ..crate::Envelope::default()
        }
    }

    #[test]
    fn a_turn_with_helpers_lists_each_allowed_harness_and_its_limits() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        let grant = envelope(&["claude-code", "codex", "no-such-harness"], 3, 1);
        mount(
            host.path(),
            Path::new("/g"),
            &skills,
            &[],
            &[],
            Some(&grant),
            None,
        )
        .unwrap();
        let tree = host.path().join(DIRECTORY);
        let text = std::fs::read_to_string(tree.join("helpers/INDEX.md")).unwrap();
        assert!(text.len() <= HELPERS_BUDGET, "{}", text.len());
        assert!(
            text.contains("at most 3 helpers at once, 1 level deep"),
            "{text}"
        );
        assert!(
            text.contains("- claude-code: Claude Code, models "),
            "{text}"
        );
        assert!(text.contains("- codex: Codex, models gpt-5.5"), "{text}");
        assert!(text.contains("- no-such-harness: a harness this server runs"));
        let index = std::fs::read_to_string(tree.join("INDEX.md")).unwrap();
        assert!(index.contains("## Helpers"), "{index}");
        assert!(index.len() <= INDEX_BUDGET);
        let output = branchyard_support::child::command("python3")
            .arg(tree.join("bin/tools"))
            .args(["search", "--kind", "helper", "codex"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(said.starts_with("helper     codex"), "{said}");
    }

    #[test]
    fn a_leaf_gets_no_helpers_file_and_a_stale_one_goes() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        let tree = host.path().join(DIRECTORY);
        for leaf in [
            None,
            Some(envelope(&["codex"], 4, 0)),
            Some(envelope(&["codex"], 0, 1)),
        ] {
            mount(
                host.path(),
                Path::new("/g"),
                &skills,
                &[],
                &[],
                leaf.as_ref(),
                None,
            )
            .unwrap();
            assert!(!tree.join("helpers/INDEX.md").exists());
            let index = std::fs::read_to_string(tree.join("INDEX.md")).unwrap();
            assert!(!index.contains("## Helpers"));
        }
        let grant = envelope(&["codex"], 2, 1);
        mount(
            host.path(),
            Path::new("/g"),
            &skills,
            &[],
            &[],
            Some(&grant),
            None,
        )
        .unwrap();
        assert!(tree.join("helpers/INDEX.md").is_file());
        let again = mount(
            host.path(),
            Path::new("/g"),
            &skills,
            &[],
            &[],
            Some(&grant),
            None,
        )
        .unwrap();
        assert!(again.written.is_empty(), "{:?}", again.written);
        mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        assert!(
            !tree.join("helpers/INDEX.md").exists(),
            "the next turn is a leaf"
        );
    }

    #[test]
    fn a_turn_with_helpers_gets_the_delegate_references_and_a_leaf_does_not() {
        let host = tempfile::tempdir().unwrap();
        let skills = staged(host.path());
        let grant = envelope(&["codex"], 2, 1);
        mount(
            host.path(),
            Path::new("/g"),
            &skills,
            &[],
            &[],
            Some(&grant),
            None,
        )
        .unwrap();
        let tree = host.path().join(DIRECTORY);
        let references = delegate_references();
        assert!(references.len() >= 10, "{}", references.len());
        let index = std::fs::read_to_string(tree.join("helpers/INDEX.md")).unwrap();
        assert!(index.len() <= HELPERS_BUDGET, "{}", index.len());
        assert!(
            index.contains("helpers/delegate/references/NAME.md"),
            "{index}"
        );
        for (name, text) in &references {
            let staged = tree.join(DELEGATE_REFERENCES).join(format!("{name}.md"));
            assert_eq!(std::fs::read_to_string(&staged).unwrap(), *text, "{name}");
        }
        // A leaf's next turn drops them.
        mount(host.path(), Path::new("/g"), &skills, &[], &[], None, None).unwrap();
        for (name, _) in references {
            assert!(!tree
                .join(DELEGATE_REFERENCES)
                .join(format!("{name}.md"))
                .exists());
        }
    }

    #[test]
    fn helpers_with_no_harness_named_run_the_turns_own_and_a_long_list_stays_small() {
        let own = render_helpers(&envelope(&[], 4, 1));
        assert!(own.contains("run your own harness"), "{own}");
        let many: Vec<String> = (0..200).map(|n| format!("harness-{n}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let long = render_helpers(&envelope(&refs, 16, 1));
        assert!(long.len() <= HELPERS_BUDGET, "{}", long.len());
        assert!(
            long.contains(" more: `bin/tools search --kind helper WORDS`"),
            "{long}"
        );
        assert!(render_helpers(&envelope(&["codex"], 1, 2))
            .contains("at most 1 helper at once, 2 levels deep"));
    }

    /// R2: the index names the chosen skills and counts the mounted ones in
    /// its overflow line, which stays within the budget whatever is mounted,
    /// and a chosen skill the budget cut joins the count.
    #[test]
    fn the_index_names_the_chosen_skills_and_counts_the_mounted_ones() {
        let host = tempfile::tempdir().unwrap();
        let all = crate::capabilities::stage(host.path(), None, None, None).unwrap();
        let (chosen, mounted): (Vec<Tool>, Vec<Tool>) = all
            .listed()
            .into_iter()
            .partition(|t| t.id == "find-tools" || t.id == "docx");
        let mounted_count = mounted.len();
        let first = mount(
            host.path(),
            Path::new("/g"),
            &chosen,
            &mounted,
            &[],
            None,
            None,
        )
        .unwrap();
        let tree = host.path().join(DIRECTORY);
        let index = std::fs::read_to_string(tree.join("INDEX.md")).unwrap();
        assert!(index.len() <= INDEX_BUDGET);
        assert!(index.contains("- docx: Create, inspect"), "{index}");
        assert!(index.contains("- find-tools:"), "{index}");
        assert!(
            !index.contains("- pptx"),
            "a mounted skill is counted, not named: {index}"
        );
        assert!(
            index.contains(&format!(
                "- ...{mounted_count} more: `bin/tools search WORDS`\n"
            )),
            "{index}"
        );
        // Mounted skills are in the tree, so the script finds them, below a chosen one of equal match.
        assert!(tree.join("skills/pptx/SKILL.md").is_file());
        let output = branchyard_support::child::command("python3")
            .arg(tree.join("bin/tools"))
            .args(["search", "pptx", "slides"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(said.contains("skill      pptx"), "{said}");
        // Nothing mounted: no overflow line, as before R2.
        mount(host.path(), Path::new("/g"), &chosen, &[], &[], None, None).unwrap();
        let index = std::fs::read_to_string(tree.join("INDEX.md")).unwrap();
        assert!(!index.contains("more:"), "{index}");
        assert!(!first.written.is_empty());
        // Many mounted and many chosen: the budget holds, and the count carries the cut.
        let many: Vec<Tool> = (0..600)
            .map(|n| tool(&format!("skill-{n:03}"), &"word ".repeat(60), "x"))
            .collect();
        let text = render_index(&many, 2_000, &[], false);
        assert!(text.len() <= INDEX_BUDGET, "{}", text.len());
        let line = text
            .lines()
            .find(|l| l.starts_with("- ..."))
            .expect("an overflow line");
        let counted: usize = line
            .trim_start_matches("- ...")
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(counted > 2_000, "{line}");
        let named = text.lines().filter(|l| l.starts_with("- skill-")).count();
        assert_eq!(named + counted, 600 + 2_000);
    }

    /// A model-surface test: the harness's own skills directory is context
    /// it reads whole, so it holds the chosen skills and none of the mounted
    /// ones, whatever the tree stages.
    #[test]
    fn the_native_skills_directory_holds_the_chosen_skills_only() {
        let home = tempfile::tempdir().unwrap();
        let all = crate::capabilities::stage(home.path(), None, None, None).unwrap();
        let (chosen, mounted): (Vec<Tool>, Vec<Tool>) =
            all.listed().into_iter().partition(|t| t.id == "docx");
        assert!(mounted.len() > 10);
        link_native(home.path(), "claude-code", &chosen).unwrap();
        let native = home.path().join(".claude/skills");
        let mut names: Vec<String> = std::fs::read_dir(&native)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["docx"]);
        assert!(home
            .path()
            .join(DIRECTORY)
            .parent()
            .unwrap()
            .join("capabilities/pptx/SKILL.md")
            .is_file());
        // A later turn that chooses another set relinks to exactly that set.
        let (next, _): (Vec<Tool>, Vec<Tool>) = all
            .listed()
            .into_iter()
            .partition(|t| t.id == "pptx" || t.id == "artifact-tools");
        let changed = link_native(home.path(), "claude-code", &next).unwrap();
        assert_eq!(
            changed,
            [
                ".claude/skills/artifact-tools",
                ".claude/skills/docx",
                ".claude/skills/pptx"
            ]
        );
        let mut names: Vec<String> = std::fs::read_dir(&native)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["artifact-tools", "pptx"]);
    }
}
