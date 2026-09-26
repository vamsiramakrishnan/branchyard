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
}

const fn harness(id: &'static str) -> Harness {
    Harness {
        id,
        target: None,
        herdr_manifest: None,
        herdr_resume: None,
        scion: None,
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
}

/// Every harness named by a vendored source or the integration matrix.
pub const HARNESSES: &[Harness] = &[
    // Integration targets, in the order of docs/harness-integration.md.
    harness("claude-code")
        .target("Claude Code")
        .herdr_manifest("claude")
        .herdr_resume("herdr:claude", "claude")
        .scion("claude"),
    harness("codex")
        .target("Codex")
        .herdr_manifest("codex")
        .herdr_resume("herdr:codex", "codex")
        .scion("codex"),
    harness("antigravity")
        .target("Antigravity")
        .herdr_manifest("agy")
        .herdr_resume("herdr:antigravity_cli", "agy")
        .scion("antigravity"),
    harness("oh-my-pi")
        .target("Oh My Pi")
        .herdr_resume("herdr:omp", "omp"),
    harness("deepseek-harness").target("DeepSeek Harness"),
    harness("gemini-cli")
        .target("Gemini CLI")
        .herdr_manifest("gemini")
        .scion("gemini-cli"),
    harness("opencode")
        .target("OpenCode")
        .herdr_manifest("opencode")
        .herdr_resume("herdr:opencode", "opencode")
        .scion("opencode"),
    harness("pi")
        .target("Pi")
        .herdr_manifest("pi")
        .herdr_resume("herdr:pi", "pi"),
    harness("goose").target("Goose"),
    harness("aider").target("Aider"),
    harness("cursor")
        .target("Cursor CLI")
        .herdr_manifest("cursor")
        .herdr_resume("herdr:cursor", "cursor"),
    harness("github-copilot")
        .target("GitHub Copilot CLI")
        .herdr_manifest("copilot")
        .herdr_resume("herdr:copilot", "copilot")
        .scion("copilot"),
    harness("amp").target("Amp").herdr_manifest("amp"),
    harness("qwen-code")
        .target("Qwen Code")
        .herdr_manifest("qwen")
        .herdr_resume("herdr:qwen", "qwen"),
    harness("kimi-cli")
        .target("Kimi CLI")
        .herdr_manifest("kimi")
        .herdr_resume("herdr:kimi", "kimi"),
    harness("hermes")
        .target("Hermes")
        .herdr_manifest("hermes")
        .herdr_resume("herdr:hermes", "hermes")
        .scion("hermes"),
    // Named by vendored sources; not integration targets.
    harness("cline").herdr_manifest("cline"),
    harness("devin")
        .herdr_manifest("devin")
        .herdr_resume("herdr:devin", "devin"),
    harness("droid")
        .herdr_manifest("droid")
        .herdr_resume("herdr:droid", "droid"),
    harness("grok-build")
        .herdr_manifest("grok")
        .herdr_resume("herdr:grok", "grok")
        .scion("grok-build"),
    harness("kilo")
        .herdr_manifest("kilo")
        .herdr_resume("herdr:kilo", "kilo"),
    harness("kiro").herdr_manifest("kiro"),
    harness("letta")
        .herdr_manifest("letta")
        .herdr_resume("herdr:letta", "letta"),
    harness("maki").herdr_manifest("maki"),
    harness("mastracode").herdr_resume("herdr:mastracode", "mastracode"),
    harness("muse-code")
        .herdr_manifest("muse")
        .scion("muse-code"),
    harness("qodercli")
        .herdr_manifest("qodercli")
        .herdr_resume("herdr:qodercli", "qodercli"),
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
        ] {
            let values: Vec<_> = HARNESSES.iter().filter_map(key).collect();
            let unique: BTreeSet<_> = values.iter().collect();
            assert_eq!(
                values.len(),
                unique.len(),
                "a source name maps to two harnesses"
            );
        }
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
