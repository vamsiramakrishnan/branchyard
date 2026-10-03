//! One identity per harness across the vendored controls.
//!
//! Herdr, Scion and Branchyard's own integration matrix name the same
//! harnesses differently: Herdr's `grok` manifest is Scion's `grok-build`, and
//! Herdr's `copilot` manifest lives in `github-copilot.toml`. This registry
//! maps every upstream name to one Branchyard ID. Tests fail when a vendored
//! source adds a name the registry does not map, or when upstream aliases
//! contradict a mapping.
//!
//! An entry records naming only. It does not mean a harness is supported;
//! support belongs to a qualified profile.

/// One harness and the names each source uses for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Harness {
    /// Branchyard's identifier.
    pub id: &'static str,
    /// Row name in `docs/harness-integration.md`, for integration targets.
    pub target: Option<&'static str>,
    /// `id` in Herdr's detection manifest.
    pub herdr_manifest: Option<&'static str>,
    /// `(source, agent)` accepted by Herdr's resume recipes.
    pub herdr_resume: Option<(&'static str, &'static str)>,
    /// Scion harness directory.
    pub scion: Option<&'static str>,
    /// Plugin `id` in emdash's agent registry
    /// (`packages/plugins/src/agents/impl/<id>`).
    pub emdash: Option<&'static str>,
    /// Keys of Orca's `TUI_AGENT_CONFIG` (`src/shared/tui-agent-config.ts`);
    /// several when Orca launches one harness more than one way.
    pub orca: &'static [&'static str],
}

const fn harness(id: &'static str) -> Harness {
    Harness {
        id,
        target: None,
        herdr_manifest: None,
        herdr_resume: None,
        scion: None,
        emdash: None,
        orca: &[],
    }
}

impl Harness {
    const fn target(mut self, name: &'static str) -> Self {
        self.target = Some(name);
        self
    }
    const fn herdr_manifest(mut self, id: &'static str) -> Self {
        self.herdr_manifest = Some(id);
        self
    }
    const fn herdr_resume(mut self, source: &'static str, agent: &'static str) -> Self {
        self.herdr_resume = Some((source, agent));
        self
    }
    const fn scion(mut self, directory: &'static str) -> Self {
        self.scion = Some(directory);
        self
    }
    const fn emdash(mut self, id: &'static str) -> Self {
        self.emdash = Some(id);
        self
    }
    const fn orca(mut self, keys: &'static [&'static str]) -> Self {
        self.orca = keys;
        self
    }
}

/// Every harness named by a vendored source or the integration matrix.
pub const HARNESSES: &[Harness] = &[
    // Integration targets, in the order of docs/harness-integration.md.
    harness("claude-code")
        .target("Claude Code")
        .herdr_manifest("claude")
        .herdr_resume("herdr:claude", "claude")
        .scion("claude")
        .emdash("claude")
        // Agent Teams is an Orca launch mode of the same `claude` binary.
        .orca(&["claude", "claude-agent-teams"]),
    harness("codex")
        .target("Codex")
        .herdr_manifest("codex")
        .herdr_resume("herdr:codex", "codex")
        .scion("codex")
        .emdash("codex")
        .orca(&["codex"]),
    harness("antigravity")
        .target("Antigravity")
        .herdr_manifest("agy")
        .herdr_resume("herdr:antigravity_cli", "agy")
        .scion("antigravity")
        .emdash("antigravity")
        .orca(&["antigravity"]),
    harness("oh-my-pi")
        .target("Oh My Pi")
        .herdr_resume("herdr:omp", "omp")
        .emdash("oh-my-pi")
        .orca(&["omp"]),
    harness("deepseek-harness")
        .target("DeepSeek Harness")
        .orca(&["dsh"]),
    harness("gemini-cli")
        .target("Gemini CLI")
        .herdr_manifest("gemini")
        .scion("gemini-cli")
        .orca(&["gemini"]),
    harness("opencode")
        .target("OpenCode")
        .herdr_manifest("opencode")
        .herdr_resume("herdr:opencode", "opencode")
        .scion("opencode")
        .emdash("opencode")
        // `opencode2` is OpenCode 2's beta binary, with the same flags.
        .orca(&["opencode", "opencode2"]),
    harness("pi")
        .target("Pi")
        .herdr_manifest("pi")
        .herdr_resume("herdr:pi", "pi")
        .emdash("pi")
        .orca(&["pi"]),
    harness("goose")
        .target("Goose")
        .emdash("goose")
        .orca(&["goose"]),
    harness("aider").target("Aider").orca(&["aider"]),
    harness("cursor")
        .target("Cursor CLI")
        .herdr_manifest("cursor")
        .herdr_resume("herdr:cursor", "cursor")
        .emdash("cursor")
        .orca(&["cursor"]),
    harness("github-copilot")
        .target("GitHub Copilot CLI")
        .herdr_manifest("copilot")
        .herdr_resume("herdr:copilot", "copilot")
        .scion("copilot")
        .emdash("copilot")
        .orca(&["copilot"]),
    harness("amp")
        .target("Amp")
        .herdr_manifest("amp")
        .emdash("amp")
        .orca(&["amp"]),
    harness("qwen-code")
        .target("Qwen Code")
        .herdr_manifest("qwen")
        .herdr_resume("herdr:qwen", "qwen")
        .emdash("qwen")
        .orca(&["qwen-code"]),
    harness("kimi-cli")
        .target("Kimi CLI")
        .herdr_manifest("kimi")
        .herdr_resume("herdr:kimi", "kimi")
        .emdash("kimi")
        .orca(&["kimi"]),
    harness("hermes")
        .target("Hermes")
        .herdr_manifest("hermes")
        .herdr_resume("herdr:hermes", "hermes")
        .scion("hermes")
        .emdash("hermes")
        .orca(&["hermes"]),
    // Named by vendored sources; not integration targets.
    harness("cline")
        .herdr_manifest("cline")
        .emdash("cline")
        .orca(&["cline"]),
    harness("devin")
        .herdr_manifest("devin")
        .herdr_resume("herdr:devin", "devin")
        .emdash("devin")
        .orca(&["devin"]),
    harness("droid")
        .herdr_manifest("droid")
        .herdr_resume("herdr:droid", "droid")
        .emdash("droid")
        .orca(&["droid"]),
    harness("grok-build")
        .herdr_manifest("grok")
        .herdr_resume("herdr:grok", "grok")
        .scion("grok-build")
        .emdash("grok")
        .orca(&["grok"]),
    harness("kilo")
        .herdr_manifest("kilo")
        .herdr_resume("herdr:kilo", "kilo")
        .emdash("kilocode")
        .orca(&["kilo"]),
    harness("kiro")
        .herdr_manifest("kiro")
        .emdash("kiro")
        .orca(&["kiro"]),
    harness("letta")
        .herdr_manifest("letta")
        .herdr_resume("herdr:letta", "letta")
        .emdash("letta"),
    harness("maki").herdr_manifest("maki"),
    harness("mastracode").herdr_resume("herdr:mastracode", "mastracode"),
    harness("muse-code")
        .herdr_manifest("muse")
        .scion("muse-code")
        .emdash("muse")
        .orca(&["muse"]),
    harness("qodercli")
        .herdr_manifest("qodercli")
        .herdr_resume("herdr:qodercli", "qodercli")
        .emdash("qoder")
        .orca(&["qoder"]),
    // Named by emdash's or Orca's agent registries only.
    harness("ante").orca(&["ante"]),
    harness("auggie").emdash("auggie").orca(&["aug"]),
    harness("autohand").emdash("autohand").orca(&["autohand"]),
    harness("codebuddy")
        .emdash("codebuddy")
        .orca(&["codebuddy"]),
    harness("codebuff").emdash("codebuff").orca(&["codebuff"]),
    harness("command-code")
        .emdash("commandcode")
        .orca(&["command-code"]),
    harness("continue").emdash("continue").orca(&["continue"]),
    // emdash files Crush under its maker, Charm.
    harness("crush").emdash("charm").orca(&["crush"]),
    harness("freebuff").emdash("freebuff").orca(&["freebuff"]),
    harness("jules").emdash("jules"),
    harness("junie").emdash("junie"),
    harness("mimo-code").emdash("mimocode").orca(&["mimo-code"]),
    harness("mistral-vibe")
        .emdash("mistral")
        .orca(&["mistral-vibe"]),
    harness("openclaude").orca(&["openclaude"]),
    harness("openclaw").orca(&["openclaw"]),
    harness("prime-agent")
        .emdash("prime-agent")
        .orca(&["prime-agent"]),
    harness("rovo-dev").emdash("rovo").orca(&["rovo"]),
    harness("trae").orca(&["trae"]),
    harness("zcode").orca(&["zcode"]),
    harness("zero").emdash("zero"),
];

/// Look up a harness by Branchyard ID.
pub fn by_id(id: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.id == id)
}

/// The harness a Herdr detection manifest `id` describes.
pub fn from_herdr_manifest(id: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.herdr_manifest == Some(id))
}

/// The harness a Herdr resume `(source, agent)` pair describes.
pub fn from_herdr_resume(source: &str, agent: &str) -> Option<&'static Harness> {
    HARNESSES
        .iter()
        .find(|h| h.herdr_resume == Some((source, agent)))
}

/// The harness emdash's agent plugin `id` describes.
pub fn from_emdash(id: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.emdash == Some(id))
}

/// The harness one of Orca's agent keys describes.
pub fn from_orca(key: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.orca.contains(&key))
}

/// The harness a Scion harness directory provisions.
pub fn from_scion(directory: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.scion == Some(directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn repository() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn quoted(line: &str) -> Vec<String> {
        line.split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    /// Manifest `id` to its declared aliases, read from the vendored TOML.
    fn herdr_manifests() -> BTreeMap<String, Vec<String>> {
        let directory = repository().join("vendor/herdr/src/detect/manifests");
        let mut manifests = BTreeMap::new();
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            // Top-level keys precede the first table; rule IDs come later.
            let header = text.split("\n[").next().unwrap();
            let field = |key: &str| {
                header
                    .lines()
                    .find(|line| line.starts_with(&format!("{key} =")))
                    .map(quoted)
            };
            let id = field("id").unwrap().remove(0);
            manifests.insert(id, field("aliases").unwrap_or_default());
        }
        manifests
    }

    fn scion_directories() -> BTreeSet<String> {
        fs::read_dir(repository().join("vendor/scion/harnesses"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
            .collect()
    }

    /// Pairs listed in `is_official_agent_source`, read from the source.
    fn herdr_resume_pairs() -> BTreeSet<(String, String)> {
        let source = include_str!("resume.rs");
        let start = source.find("fn is_official_agent_source").unwrap();
        let body = &source[start..start + source[start..].find("\n}").unwrap()];
        body.lines()
            .filter(|line| line.contains("(\"herdr:"))
            .map(|line| {
                let mut values = quoted(line).into_iter();
                (values.next().unwrap(), values.next().unwrap())
            })
            .collect()
    }

    fn integration_targets() -> Vec<String> {
        let text = fs::read_to_string(repository().join("docs/harness-integration.md")).unwrap();
        let section = text
            .split("## Sixteen initial integration targets")
            .nth(1)
            .unwrap();
        let table = section
            .split("\n\n")
            .find(|block| block.starts_with("| Harness"))
            .unwrap();
        table
            .lines()
            .skip(2)
            .map(|row| row.split('|').nth(1).unwrap().trim().to_owned())
            .collect()
    }

    #[test]
    fn identifiers_are_unique_slugs() {
        let mut ids = BTreeSet::new();
        for harness in HARNESSES {
            assert!(
                harness
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{} is not a lowercase slug",
                harness.id
            );
            assert!(ids.insert(harness.id), "duplicate ID {}", harness.id);
        }
        for key in [
            |h: &Harness| h.target.map(str::to_owned),
            |h: &Harness| h.herdr_manifest.map(str::to_owned),
            |h: &Harness| h.herdr_resume.map(|(s, a)| format!("{s}/{a}")),
            |h: &Harness| h.scion.map(str::to_owned),
            |h: &Harness| h.emdash.map(str::to_owned),
        ] {
            let values: Vec<_> = HARNESSES.iter().filter_map(key).collect();
            let unique: BTreeSet<_> = values.iter().collect();
            assert_eq!(
                values.len(),
                unique.len(),
                "a source name maps to two harnesses"
            );
        }
        let orca: Vec<_> = HARNESSES.iter().flat_map(|h| h.orca).collect();
        let unique: BTreeSet<_> = orca.iter().collect();
        assert_eq!(
            orca.len(),
            unique.len(),
            "an Orca key maps to two harnesses"
        );
    }

    #[test]
    fn every_herdr_manifest_is_registered() {
        let manifests = herdr_manifests();
        assert_eq!(manifests.len(), 22);
        for id in manifests.keys() {
            assert!(
                from_herdr_manifest(id).is_some(),
                "unregistered Herdr manifest {id}"
            );
        }
        for harness in HARNESSES {
            if let Some(id) = harness.herdr_manifest {
                assert!(manifests.contains_key(id), "no Herdr manifest {id}");
            }
        }
    }

    #[test]
    fn every_scion_harness_is_registered() {
        let directories = scion_directories();
        assert_eq!(directories.len(), 9);
        for directory in &directories {
            assert!(
                from_scion(directory).is_some(),
                "unregistered Scion harness {directory}"
            );
        }
        for harness in HARNESSES {
            if let Some(directory) = harness.scion {
                assert!(
                    directories.contains(directory),
                    "no Scion harness {directory}"
                );
            }
        }
    }

    #[test]
    fn every_herdr_resume_source_is_registered() {
        let pairs = herdr_resume_pairs();
        assert_eq!(pairs.len(), 18);
        for (source, agent) in &pairs {
            assert!(
                from_herdr_resume(source, agent).is_some(),
                "unregistered Herdr resume source {source}/{agent}"
            );
        }
        for harness in HARNESSES {
            if let Some((source, agent)) = harness.herdr_resume {
                assert!(
                    crate::resume::is_official_agent_source(source, agent),
                    "Herdr does not accept {source}/{agent}"
                );
            }
        }
    }

    #[test]
    fn every_integration_target_is_registered_once() {
        let targets = integration_targets();
        assert_eq!(targets.len(), 16);
        let registered: Vec<_> = HARNESSES.iter().filter_map(|h| h.target).collect();
        assert_eq!(
            registered, targets,
            "registry order or names differ from the matrix"
        );
    }

    /// Every agent emdash's plugin registry ships is mapped, and every
    /// mapping names a plugin that exists, with the `id` its directory says.
    #[test]
    fn every_emdash_agent_is_registered() {
        let agents = crate::upstream::emdash_agents();
        assert_eq!(agents.len(), 37);
        for (directory, agent) in &agents {
            let id = agent.meta["id"].as_str().unwrap();
            assert_eq!(id, directory, "emdash plugin {directory} declares id {id}");
            assert!(
                from_emdash(id).is_some(),
                "unregistered emdash agent {id}: map it in HARNESSES"
            );
        }
        for harness in HARNESSES {
            if let Some(id) = harness.emdash {
                assert!(agents.contains_key(id), "no emdash agent {id}");
            }
        }
    }

    /// Every agent Orca's launcher table knows is mapped, its `TuiAgent`
    /// union lists the same keys, and every mapping names a key that exists.
    #[test]
    fn every_orca_agent_is_registered() {
        let agents = crate::upstream::orca_agents();
        let names = crate::upstream::orca_agent_names();
        assert_eq!(agents.len(), 43);
        assert_eq!(
            agents.keys().collect::<Vec<_>>(),
            names.keys().collect::<Vec<_>>(),
            "Orca's TuiAgent union and TUI_AGENT_CONFIG disagree"
        );
        for key in agents.keys() {
            assert!(
                from_orca(key).is_some(),
                "unregistered Orca agent {key}: map it in HARNESSES"
            );
        }
        for harness in HARNESSES {
            for key in harness.orca {
                assert!(agents.contains_key(*key), "no Orca agent {key}");
            }
        }
    }

    /// Where both registries know a harness, they agree on its executable,
    /// which is the evidence a mapping rests on.
    #[test]
    fn emdash_and_orca_agree_on_each_mapped_executable() {
        let emdash = crate::upstream::emdash_agents();
        let orca = crate::upstream::orca_agents();
        // Known disagreements, each with why the mapping still holds.
        const EXCEPTIONS: &[(&str, &str)] = &[(
            "rovo-dev",
            "both name Atlassian's Rovo Dev CLI (homepage and display name); emdash runs it \
             through `acli rovodev`, Orca looks for a `rovo` executable",
        )];
        let mut checked = 0;
        for harness in HARNESSES {
            let (Some(id), [key, ..]) = (harness.emdash, harness.orca) else {
                continue;
            };
            if EXCEPTIONS.iter().any(|(e, _)| *e == harness.id) {
                continue;
            }
            let binaries = crate::catalog::tests::emdash_binaries(&emdash[id]);
            let config = &orca[*key];
            let mut detect = vec![config["detectCmd"].as_str().unwrap().to_owned()];
            for alias in config["detectCmdAliases"].as_array().into_iter().flatten() {
                detect.push(alias.as_str().unwrap().to_owned());
            }
            assert!(
                binaries.iter().any(|b| detect.contains(b)),
                "{}: emdash runs {binaries:?}, Orca detects {detect:?}",
                harness.id
            );
            checked += 1;
        }
        assert!(checked >= 30, "only {checked} cross-checked");
    }

    /// Herdr's aliases are upstream evidence about identity. An alias equal to
    /// another source's name must resolve to the same harness.
    #[test]
    fn upstream_aliases_agree_with_the_registry() {
        let mut checked = 0;
        for (id, aliases) in herdr_manifests() {
            let owner = from_herdr_manifest(&id).unwrap();
            for alias in aliases {
                let others = [by_id(&alias), from_scion(&alias)];
                for other in others.into_iter().flatten() {
                    assert_eq!(
                        other.id, owner.id,
                        "Herdr alias {alias} of {id} names {}",
                        other.id
                    );
                    checked += 1;
                }
            }
        }
        // claude-code, github-copilot, grok-build and muse-code are all
        // confirmed this way; losing one means the evidence changed.
        assert!(checked >= 4, "only {checked} aliases cross-checked");
    }
}
