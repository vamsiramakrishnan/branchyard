//! How a command reaches a recipe's machine: `ssh` to the result's target,
//! or the result's own argument vector (`docker exec -i NAME`), each
//! followed by `sh -c SCRIPT`.

use std::process::Command;

use crate::{Connection, SshTarget};

/// One machine's way in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Ssh { program: String, target: SshTarget },
    Exec { argv: Vec<String> },
}

/// One shell word, quoted for a POSIX shell (plain words stay as they are).
/// Every layer that builds a remote script quotes through here.
pub fn quote(word: &str) -> String {
    // Only a NUL byte cannot be quoted, and no shell word can hold one.
    shlex::try_quote(&word.replace('\0', ""))
        .map(|quoted| quoted.into_owned())
        .unwrap_or_default()
}

impl Transport {
    /// The transport for `connection`, with `ssh` as the ssh program.
    pub fn new(connection: &Connection, ssh: &str) -> Transport {
        match connection {
            Connection::Ssh { target, .. } => Transport::Ssh {
                program: ssh.to_owned(),
                target: target.clone(),
            },
            Connection::Exec { argv, .. } => Transport::Exec { argv: argv.clone() },
        }
    }

    /// A command that runs `script` with `sh -c` on the machine, with
    /// `args` as `$1`, `$2`, ... Never prompts: ssh runs in batch mode.
    pub fn command(&self, script: &str, args: &[&str]) -> Command {
        match self {
            Transport::Ssh { program, target } => {
                let mut command = Command::new(program);
                command.args(["-T", "-o", "BatchMode=yes"]);
                if let Some(identity) = &target.identity_file {
                    command.arg("-i").arg(identity);
                }
                if target.identities_only == Some(true) {
                    command.args(["-o", "IdentitiesOnly=yes"]);
                }
                let host = match &target.config_host {
                    Some(alias) => alias.clone(),
                    None => {
                        if target.port != 22 {
                            command.arg("-p").arg(target.port.to_string());
                        }
                        if !target.username.is_empty() {
                            command.arg("-l").arg(&target.username);
                        }
                        target.host.clone()
                    }
                };
                // The remote login shell parses one string: every word quoted.
                let mut remote = format!("sh -c {} sh", quote(script));
                for arg in args {
                    remote.push(' ');
                    remote.push_str(&quote(arg));
                }
                command.arg("--").arg(host).arg(remote);
                command
            }
            Transport::Exec { argv } => {
                let mut command = Command::new(&argv[0]);
                command
                    .args(&argv[1..])
                    .args(["sh", "-c", script, "sh"])
                    .args(args);
                command
            }
        }
    }

    /// For messages.
    pub fn describe(&self) -> String {
        match self {
            Transport::Ssh { target, .. } => {
                let label = target.label.as_deref().unwrap_or("");
                let host = target.config_host.as_deref().unwrap_or(&target.host);
                let user = match target.username.is_empty() {
                    true => String::new(),
                    false => format!("{}@", target.username),
                };
                format!(
                    "ssh {user}{host}:{}{}",
                    target.port,
                    match label {
                        "" => String::new(),
                        l => format!(" ({l})"),
                    }
                )
            }
            Transport::Exec { argv } => argv.join(" "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            label: None,
            config_host: None,
            host: "10.0.0.7".into(),
            port: 2222,
            username: "dev".into(),
            identity_file: Some("/k/id".into()),
            identities_only: Some(true),
        }
    }

    #[test]
    fn ssh_commands_quote_one_remote_string() {
        let transport = Transport::Ssh {
            program: "ssh".into(),
            target: target(),
        };
        let command = transport.command("echo \"$1\"", &["it's"]);
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let (remote, ssh_args) = args.split_last().unwrap();
        assert_eq!(
            ssh_args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-i",
                "/k/id",
                "-o",
                "IdentitiesOnly=yes",
                "-p",
                "2222",
                "-l",
                "dev",
                "--",
                "10.0.0.7",
            ]
        );
        // One string the remote shell splits back into `sh -c SCRIPT sh ARG`.
        assert_eq!(
            shlex::split(remote).unwrap(),
            ["sh", "-c", "echo \"$1\"", "sh", "it's"]
        );
        assert_eq!(transport.describe(), "ssh dev@10.0.0.7:2222");
    }

    #[test]
    fn exec_commands_append_sh_c() {
        let transport = Transport::Exec {
            argv: vec!["docker".into(), "exec".into(), "-i".into(), "vm".into()],
        };
        let command = transport.command("true", &["a"]);
        assert_eq!(command.get_program(), "docker");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["exec", "-i", "vm", "sh", "-c", "true", "sh", "a"]);
    }
}
