//! The Branchyard skills, embedded byte for byte from
//! `plugins/branchyard/skills/`, so `by init plugin` installs exactly what
//! the plugin ships without a checkout. A test compares them with the
//! files on disk.

/// A skill's files, by path relative to the skill's directory.
pub struct Skill {
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! skill_file {
    ($skill:literal, $path:literal) => {
        (
            $path,
            include_str!(concat!(
                "../../../plugins/branchyard/skills/",
                $skill,
                "/",
                $path
            )),
        )
    };
}

pub const SKILLS: &[Skill] = &[
    Skill {
        name: "setup",
        files: &[
            skill_file!("setup", "SKILL.md"),
            skill_file!("setup", "agents/openai.yaml"),
            skill_file!("setup", "references/deploy-and-plugin.md"),
            skill_file!("setup", "references/project.md"),
            skill_file!("setup", "references/protocol.md"),
            skill_file!("setup", "references/rig.md"),
            skill_file!("setup", "references/server.md"),
        ],
    },
    Skill {
        name: "delegate",
        files: &[
            skill_file!("delegate", "SKILL.md"),
            skill_file!("delegate", "agents/openai.yaml"),
        ],
    },
];

pub fn skill(name: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|s| s.name == name)
}

/// A `SKILL.md` needs frontmatter with a `name` and a `description`; other
/// files need nothing.
pub fn check_skill_file(path: &str, text: &str) -> Result<(), String> {
    if !path.ends_with("SKILL.md") {
        return Ok(());
    }
    let front = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(front, _)| front)
        .ok_or("SKILL.md needs YAML frontmatter between --- lines")?;
    for key in ["name", "description"] {
        let found = front.lines().any(|line| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':') && rest.len() > 2)
        });
        if !found {
            return Err(format!("SKILL.md frontmatter needs a {key}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn embedded_skills_are_the_shipped_files_and_nothing_is_missing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/branchyard/skills");
        for skill in SKILLS {
            let dir = root.join(skill.name);
            let mut on_disk = Vec::new();
            let mut stack = vec![dir.clone()];
            while let Some(d) = stack.pop() {
                for entry in std::fs::read_dir(&d).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        on_disk.push(
                            path.strip_prefix(&dir)
                                .unwrap()
                                .to_string_lossy()
                                .replace('\\', "/"),
                        );
                    }
                }
            }
            on_disk.sort();
            let embedded: Vec<&str> = skill.files.iter().map(|(p, _)| *p).collect();
            assert_eq!(
                on_disk, embedded,
                "skills/{} changed; update SKILLS",
                skill.name
            );
            for (path, text) in skill.files {
                assert_eq!(std::fs::read_to_string(dir.join(path)).unwrap(), *text);
                check_skill_file(path, text).unwrap();
            }
        }
    }
}
