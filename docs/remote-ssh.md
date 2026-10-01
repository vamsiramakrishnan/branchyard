# Remote over ssh

`by --remote ssh://[user@]host[:port]/path/to/repo` runs every command against a repository on another machine you can `ssh` to, with nothing to configure there beyond having `by` on its `PATH`: no TCP listener, no TLS, no token to copy.

```sh
by --remote ssh://me@build.example/srv/app run "Make the flaky parser test deterministic" --check "cargo test" --yes
by --remote ssh://me@build.example/srv/app ls
export BRANCHYARD_REMOTE=ssh://me@build.example/srv/app   # or [remote] url in branchyard.toml
by watch
by remote ssh status            # the connection and the remote server
by remote ssh stop              # stop the remote server and close the connection
```

`ssh://host/~/src/app` names a repository under the remote home. Every `--remote` command works, with the output of an `http(s)://` server ([server](server.md#remote-mode-in-by)): it is the same client talking to the same `by serve`, only over a different socket.

## What happens

The first command for a URL:

1. **Connects once.** The system `ssh` starts a control master (`ssh -M -N -f -o ControlPersist=600`), authenticating as your `ssh` configuration says (keys, agent, `~/.ssh/config`, `ProxyJump`, and a prompt on the terminal if it must). Later commands reuse it; it exits ten minutes after the last one.
2. **Starts the server** (or finds it running). One POSIX `sh` script, sent on the channel's stdin, makes `~/.branchyard/remote/<key>/` (mode 0700; `<key>` is a digest of the repository path) with `run/` inside it, generates a token there (`token`, 0600, from `/dev/urandom`) if there is none, and starts `by serve --listen-unix ~/.branchyard/remote/<key>/run/by.sock --token-file …/token` in the repository with `nohup`, waiting until the socket exists. `--listen-unix` serves plain HTTP on a Unix socket, mode 0600, and refuses a directory anyone but its owner can enter. The layout follows emdash's workspace server ([ports](vendoring.md#emdash-and-orca-ports-as-data-and-as-translations)).
3. **Fetches the token** on the same channel's stdout into a 0600 file here. It is never on any command line, here or there.
4. **Forwards the socket**: `ssh -O forward -L <here>/by.sock:<there>/run/by.sock`, through the master, so nothing listens on a TCP port on either machine.
5. **Talks to it** with `branchyard-client` at `unix:<here>/by.sock`, the transport this added (`Client::new("unix:/path/to/socket", token)`).

Local state is one private directory per URL: `$BRANCHYARD_SSH_DIR`, else `$XDG_RUNTIME_DIR/branchyard-ssh`, else `~/.branchyard/ssh`, then a digest of the URL, holding the control socket, the forwarded socket, the token and the master's log. A socket path must stay under 100 bytes; set `BRANCHYARD_SSH_DIR` to something short if your home is deep.

| Variable | Default | Meaning |
|---|---|---|
| `BRANCHYARD_SSH` | `ssh` | The ssh program |
| `BRANCHYARD_SSH_BY` | `by` | The `by` to run on the host, if it is not on the non-interactive `PATH` |
| `BRANCHYARD_SSH_SERVE_ARGS` | none | More `by serve` arguments for a server this starts, such as `--allow-delegation` (split on whitespace) |
| `BRANCHYARD_SSH_DIR` | see above | Where the local state lives |

The remote server also reads the repository's own `[serve] config` from its `branchyard.toml` ([setup](setup.md)). Flags apply when the server starts; `by remote ssh stop` and the next command restarts it with new ones.

## Trust and isolation

The server runs as your user on the host, with **no isolation** beyond it, like any `by serve` with the local provider ([server](server.md#security)). Only someone who can already log in as you there can reach the socket or read the token. `--token-file` and `--ca-file` are refused with an `ssh://` URL, since it brings its own. `by remote ssh stop` stops the server (SIGTERM, which drains as [running it](server.md#running-it) describes, waiting up to 90 seconds), closes the master and forgets the local socket and token; the remote token stays, so the next start reuses it, and the branches and their state stay in the repository's `.branchyard/`.

## Tested

`crates/branchyard-cli/tests/remote_ssh.rs` runs the whole path hermetically, with `crates/branchyard-recipe/tests/fixtures/fake-ssh` in place of `ssh`: a Python stand-in that runs each "remote" command here in a session of its own with a separate remote `HOME`, keeps a control master as a background process, and implements `-O check|forward|exit` with a small Unix socket proxy. The test checks the master starts once and is reused, the remote directory, run directory, token and socket modes, that the token reached no argv, `by run` with the fake ACP agent, `ls`, `log` and `diff` through the forward, the server reused by pid, `by remote ssh status` (text and JSON), `stop`, and a restart that keeps the branches; and the refusals (an unknown host, a missing directory, an option-shaped host, `--token-file`). `crates/branchyard-server` tests `--listen-unix`'s private-directory rule and stale-socket replacement; `branchyard-client` parses `unix:` endpoints.

**Not tested:** a real `sshd`. None is installed in this environment (no `sshd`, nor even an `ssh` client), so OpenSSH's own behaviour, such as `-O forward` of a stream-local socket, `StreamLocalBindUnlink`, `ControlPersist` and a remote login shell other than `sh`, is exercised only as the fake models it.
