//! Provider logins: a harness reaching its model through a provider other
//! than its own, with a key of that provider's, such as Claude Code on
//! Z.ai's GLM models through Z.ai's Anthropic-compatible endpoint.
//!
//! Each [`ProviderLogin`] is a row of data: the secret the key is stored
//! as (one per provider, shared by every harness logged in to it, so
//! `ZAI_API_KEY` serves Claude Code, Codex and Pi alike), the harness's own
//! variables it becomes, the model a run gets when it names none, and the
//! harness's own configuration that selects the provider where a variable
//! cannot. `by harnesses login ID --provider NAME` stores the key and
//! records the choice in `[logins]`; a branch with a private home carries
//! it in [`crate::Provisioning::logins`], and [`crate::plan`] gives the
//! harness's provisioner the login's variables as if they had been given
//! as secrets ([`derive()`]), so each provisioner places them its own way
//! (Claude Code's token through `apiKeyHelper`, Codex's provider in
//! `config.toml`), and records the delivery under the provider's key
//! ([`finish`]). A run whose home is yours gets nothing from here: `by
//! harnesses login` prints the `export` line ([`exports`]) and offers the
//! configuration change with its diff.
//!
//! Endpoints and models are the ones round 8 ran these harnesses on (25
//! October 2026, `docs/qualification/README.md`): Z.ai serves the same key
//! on its Anthropic Messages endpoint, its GLM Coding Plan chat completions
//! endpoint and its Responses endpoint.

use crate::edit::{path, Edit, JsonEdit, TomlEdit};
use crate::{toml, Context, EnvVar, Plan, Secret, Via};

/// Z.ai's Anthropic Messages endpoint, for Claude Code.
pub const ZAI_ANTHROPIC: &str = "https://api.z.ai/api/anthropic";
/// Z.ai's GLM Coding Plan chat completions endpoint, for OpenAI-compatible
/// harnesses.
pub const ZAI_CODING: &str = "https://api.z.ai/api/coding/paas/v4";
/// Z.ai's Responses endpoint, the only wire API Codex takes.
pub const ZAI_RESPONSES: &str = "https://api.z.ai/api/v1";

/// A model provider a harness can be logged in to with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provider {
    /// What `--provider` takes: `zai`.
    pub name: &'static str,
    /// For people: `Z.ai (GLM)`.
    pub title: &'static str,
    /// The secret the key is stored and provisioned as: `ZAI_API_KEY`.
    pub key: &'static str,
}

/// Every provider a login can name.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        name: "zai",
        title: "Z.ai (GLM)",
        key: "ZAI_API_KEY",
    },
    Provider {
        name: "gemini",
        title: "Google's Gemini API",
        key: "GEMINI_API_KEY",
    },
];

/// What a login sets one of the harness's variables to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// The provider's key.
    Key,
    /// A fixed value, such as the provider's endpoint.
    Text(&'static str),
}

/// The harness's own configuration that selects the provider, where a
/// variable cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Config {
    /// A provider of Codex's own in `~/.codex/config.toml`
    /// (`[model_providers.ID]`) that `model_provider` names, reading its key
    /// from the variable `env_key`. Codex takes no base URL from its
    /// environment, so this is how it is pointed at another endpoint.
    CodexProvider {
        /// The table's name, which `model_provider` names: `zai`.
        id: &'static str,
        /// What Codex shows for it: `Z.ai`.
        name: &'static str,
        /// The endpoint, which must serve the Responses API.
        base_url: &'static str,
        /// The variable Codex reads the key from.
        env_key: &'static str,
    },
    /// One key of a JSON settings file in the home: Antigravity's
    /// `modelProvider`, which decides whether `agy` reads `GEMINI_API_KEY`,
    /// or OpenCode's `model`. Their provisioners write it into a private
    /// home; this is for a home that is yours.
    Json {
        /// The file, relative to the home.
        path: &'static str,
        /// The top-level key.
        key: &'static str,
        /// The string it is set to.
        value: &'static str,
    },
}

/// A harness logged in to a provider: the data `by harnesses login ID
/// --provider NAME` and provisioning share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderLogin {
    /// Branchyard's harness ID.
    pub harness: &'static str,
    /// [`Provider::name`].
    pub provider: &'static str,
    /// [`Provider::key`]: the secret the key is stored as.
    pub key: &'static str,
    /// The harness's own variables, set from the key or to fixed values.
    pub env: &'static [(&'static str, Value)],
    /// The model a run is given when it names none.
    pub model: Option<&'static str>,
    /// The variable the harness reads a model from, for a run in a home
    /// that is yours (the `export` line); `None` when it takes one only
    /// from `--model` or its configuration.
    pub model_env: Option<&'static str>,
    /// The harness's own configuration that selects the provider.
    pub config: Option<Config>,
}

/// Every provider login, by harness and provider.
pub const LOGINS: &[ProviderLogin] = &[
    // Claude Code 2.1.310 on GLM-4.6: qualified 9 of 9 with these
    // variables (25 October 2026). Without `ENABLE_TOOL_SEARCH=false` GLM
    // reads the deferred-tools prompt, searches for a shell tool, finds
    // none and never calls Bash (5 of 9; docs/troubleshooting.md).
    ProviderLogin {
        harness: "claude-code",
        provider: "zai",
        key: "ZAI_API_KEY",
        env: &[
            ("ANTHROPIC_AUTH_TOKEN", Value::Key),
            ("ANTHROPIC_BASE_URL", Value::Text(ZAI_ANTHROPIC)),
            ("ENABLE_TOOL_SEARCH", Value::Text("false")),
        ],
        model: Some("glm-4.6"),
        model_env: Some("ANTHROPIC_MODEL"),
        config: None,
    },
    // Qwen Code 0.26.0 takes its OpenAI-compatible login from the
    // environment only when all three are set (`getAuthTypeFromEnv`):
    // the key, the base URL and a model. glm-4.5-air qualified 9 of 9.
    ProviderLogin {
        harness: "qwen-code",
        provider: "zai",
        key: "ZAI_API_KEY",
        env: &[
            ("OPENAI_API_KEY", Value::Key),
            ("OPENAI_BASE_URL", Value::Text(ZAI_CODING)),
        ],
        model: Some("glm-4.5-air"),
        model_env: Some("OPENAI_MODEL"),
        config: None,
    },
    // codex-cli 0.164.0 on GLM-4.6 through Z.ai's Responses endpoint:
    // qualified 9 of 9 with this provider entry (25 October 2026).
    ProviderLogin {
        harness: "codex",
        provider: "zai",
        key: "ZAI_API_KEY",
        env: &[("ZAI_API_KEY", Value::Key)],
        model: Some("glm-4.6"),
        model_env: None,
        config: Some(Config::CodexProvider {
            id: "zai",
            name: "Z.ai",
            base_url: ZAI_RESPONSES,
            env_key: "ZAI_API_KEY",
        }),
    },
    // Pi 1.4.0 reads ZAI_API_KEY for its `zai` provider; glm-5.3-flash is
    // what it lists (it no longer lists glm-4.5-air). 9 of 9.
    ProviderLogin {
        harness: "pi",
        provider: "zai",
        key: "ZAI_API_KEY",
        env: &[("ZAI_API_KEY", Value::Key)],
        model: Some("zai/glm-5.3-flash"),
        model_env: None,
        config: None,
    },
    // Oh My Pi 18.12.6 likewise, on zai/glm-5.3. 9 of 9.
    ProviderLogin {
        harness: "oh-my-pi",
        provider: "zai",
        key: "ZAI_API_KEY",
        env: &[("ZAI_API_KEY", Value::Key)],
        model: Some("zai/glm-5.3"),
        model_env: None,
        config: None,
    },
    // OpenCode 1.19.4's `zai-coding-plan` provider (models.dev) reads
    // ZHIPU_API_KEY and serves the Coding Plan endpoint; ACP has no model
    // parameter, so the model is `model` in its opencode.json.
    ProviderLogin {
        harness: "opencode",
        provider: "zai",
        key: "ZAI_API_KEY",
        env: &[("ZHIPU_API_KEY", Value::Key)],
        model: Some("zai-coding-plan/glm-5.3-flash"),
        model_env: None,
        config: Some(Config::Json {
            path: ".config/opencode/opencode.json",
            key: "model",
            value: "zai-coding-plan/glm-5.3-flash",
        }),
    },
    // Gemini CLI reads GEMINI_API_KEY itself.
    ProviderLogin {
        harness: "gemini-cli",
        provider: "gemini",
        key: "GEMINI_API_KEY",
        env: &[("GEMINI_API_KEY", Value::Key)],
        model: None,
        model_env: None,
        config: None,
    },
    // Antigravity reads GEMINI_API_KEY only with `modelProvider` gemini in
    // its settings (docs/harness-lifecycle.md).
    ProviderLogin {
        harness: "antigravity",
        provider: "gemini",
        key: "GEMINI_API_KEY",
        env: &[("GEMINI_API_KEY", Value::Key)],
        model: None,
        model_env: None,
        config: Some(Config::Json {
            path: ".gemini/antigravity-cli/settings.json",
            key: "modelProvider",
            value: "gemini",
        }),
    },
    // OpenCode 1.19.4's Google SDK reads GOOGLE_GENERATIVE_AI_API_KEY, not
    // GEMINI_API_KEY (checked 25 October 2026).
    ProviderLogin {
        harness: "opencode",
        provider: "gemini",
        key: "GEMINI_API_KEY",
        env: &[("GOOGLE_GENERATIVE_AI_API_KEY", Value::Key)],
        model: None,
        model_env: None,
        config: None,
    },
];

/// The provider called `name`.
pub fn provider(name: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.name == name)
}

/// `harness` logged in to `provider`, when there is such a login.
pub fn find(harness: &str, provider: &str) -> Option<&'static ProviderLogin> {
    LOGINS
        .iter()
        .find(|l| l.harness == harness && l.provider == provider)
}

/// The providers `harness` can be logged in to, by name.
pub fn providers_of(harness: &str) -> Vec<&'static str> {
    LOGINS
        .iter()
        .filter(|l| l.harness == harness)
        .map(|l| l.provider)
        .collect()
}

/// `harness` logged in to `provider`, or why not: an unknown provider, or
/// one this harness has no login for, each with what there is.
pub fn check(harness: &str, provider: &str) -> Result<&'static ProviderLogin, String> {
    if let Some(login) = find(harness, provider) {
        return Ok(login);
    }
    let known: Vec<&str> = PROVIDERS.iter().map(|p| p.name).collect();
    if self::provider(provider).is_none() {
        return Err(format!(
            "no provider named {provider:?}; providers are {}",
            known.join(", ")
        ));
    }
    let theirs = providers_of(harness);
    Err(match theirs.is_empty() {
        true => format!(
            "{harness} has no provider logins; harnesses with one are {}",
            {
                let mut harnesses: Vec<&str> = Vec::new();
                for login in LOGINS {
                    if !harnesses.contains(&login.harness) {
                        harnesses.push(login.harness);
                    }
                }
                harnesses.join(", ")
            }
        ),
        false => format!(
            "{harness} has no {provider} login; it has {}",
            theirs.join(", ")
        ),
    })
}

/// The login `context`'s turn uses: the one its provisioning names for its
/// harness. None on the model gateway, which holds the keys and decides
/// the endpoint, and none when `--auth` chose a method explicitly.
pub fn active(context: &Context) -> Option<&'static ProviderLogin> {
    if context.model_gateway.is_some() || context.auth.is_some() {
        return None;
    }
    find(&context.harness, context.login.as_deref()?)
}

/// The context the harness's provisioner plans `login` from: the
/// credentials it would otherwise log in with (`credentials`, by name) set
/// aside, and the login's variables given as secrets in their place, the
/// key's under each variable that takes it; the login's model when the run
/// names none. Refused when the key is not among the secrets.
pub fn derive(
    context: &Context,
    login: &ProviderLogin,
    credentials: &[String],
) -> Result<Context, String> {
    let key = context.secret(login.key).ok_or_else(|| {
        format!(
            "{}'s {} login needs the secret {}; store it with `by harnesses login {} \
             --provider {}`",
            login.harness, login.provider, login.key, login.harness, login.provider
        )
    })?;
    let mut derived = context.clone();
    let taken: Vec<&str> = login.env.iter().map(|(name, _)| *name).collect();
    derived.secrets.retain(|s| {
        s.name != login.key && !taken.contains(&s.name.as_str()) && !credentials.contains(&s.name)
    });
    for (name, value) in login.env {
        if *value == Value::Key {
            derived.secrets.push(Secret::new(*name, key));
        }
    }
    if derived.model.is_none() {
        derived.model = login.model.map(str::to_owned);
    }
    Ok(derived)
}

/// Finish a plan made from [`derive()`]'s context: the key's variables the
/// provisioner did not place go into the harness's environment, the fixed
/// ones too; every delivery of the key is recorded under the provider's
/// secret; and the secrets the login set aside (`set_aside`) are reported
/// unused, since the login replaced them.
pub fn finish(plan: &mut Plan, login: &ProviderLogin, key: &str, set_aside: &[String]) {
    for (name, value) in login.env {
        let placed = plan.env.iter().any(|e| e.name == *name)
            || plan.secrets.iter().any(|d| d.secret == *name);
        match value {
            // Taken to reach tool commands: Branchyard cannot tell which
            // harnesses filter their tools' environment.
            Value::Key if !placed => plan.secret_env(login.key, name, key, true),
            Value::Key => {}
            Value::Text(text) => plan.set_env(EnvVar::plain(name, *text)),
        }
    }
    for delivery in &mut plan.secrets {
        if login
            .env
            .iter()
            .any(|(name, value)| *value == Value::Key && delivery.secret == *name)
        {
            delivery.secret = login.key.to_owned();
        }
    }
    // One record per way the key arrives.
    let mut seen: Vec<(String, Via)> = Vec::new();
    plan.secrets.retain(|d| {
        let id = (d.secret.clone(), d.via.clone());
        let fresh = !seen.contains(&id);
        seen.push(id);
        fresh
    });
    plan.unused_secrets
        .retain(|name| name != login.key && !login.env.iter().any(|(n, _)| n == name));
    for name in set_aside {
        if !plan.unused_secrets.contains(name) {
            plan.unused_secrets.push(name.clone());
        }
    }
    plan.auth = Some(login.provider.to_owned());
}

/// The `export` line for a harness run in a home that is yours: the
/// login's variables, the key as the shell expression `key` gives it
/// (`$(cat FILE)`, `$VAR`: never its value), and the model where the
/// harness reads one from a variable. The table's fixed values and models
/// are words the shell takes as they are (a test checks), so none is
/// quoted.
pub fn exports(login: &ProviderLogin, key: &str) -> String {
    let mut words = Vec::new();
    for (name, value) in login.env {
        words.push(match value {
            Value::Key => format!("{name}=\"{key}\""),
            Value::Text(text) => format!("{name}={text}"),
        });
    }
    if let (Some(var), Some(model)) = (login.model_env, login.model) {
        words.push(format!("{var}={model}"));
    }
    format!("export {}", words.join(" "))
}

/// The edits that select the provider in the harness's own configuration,
/// by path relative to the home, and for a home that is yours: Codex's
/// provider (with the login's model, which an app-server session would
/// otherwise be given), or the JSON key.
pub fn config_edit(login: &ProviderLogin) -> Option<(&'static str, Edit)> {
    match login.config? {
        Config::CodexProvider { .. } => {
            let mut edits = codex_provider_edits(login)?;
            if let Some(model) = login.model {
                edits.insert(
                    0,
                    TomlEdit::SetKey {
                        key: "model".into(),
                        literal: toml::string(model),
                    },
                );
            }
            Some((crate::codex::CONFIG, Edit::Toml(edits)))
        }
        Config::Json {
            path: file,
            key,
            value,
        } => Some((
            file,
            Edit::Json {
                edits: vec![JsonEdit::Set(path(&[key]), serde_json::json!(value))],
                comment_lines: false,
            },
        )),
    }
}

/// A file's text after `edit` (from [`config_edit`]), given its current
/// text (`None`: no file yet).
pub fn apply(edit: &Edit, current: Option<&str>) -> Result<String, String> {
    crate::edit::apply_all(std::slice::from_ref(edit), current).map(Option::unwrap_or_default)
}

/// The `config.toml` edits for a [`Config::CodexProvider`] login:
/// `model_provider` names it, and its table is written whole.
pub fn codex_provider_edits(login: &ProviderLogin) -> Option<Vec<TomlEdit>> {
    let Some(Config::CodexProvider {
        id,
        name,
        base_url,
        env_key,
    }) = login.config
    else {
        return None;
    };
    Some(vec![
        TomlEdit::RemoveTables(format!("model_providers.{id}")),
        TomlEdit::SetKey {
            key: "model_provider".into(),
            literal: toml::string(id),
        },
        TomlEdit::AppendTable(format!(
            "[model_providers.{id}]\nname = {}\nbase_url = {}\nenv_key = {}\nwire_api = \
             \"responses\"\n",
            toml::string(name),
            toml::string(base_url),
            toml::string(env_key),
        )),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protocol;

    #[test]
    fn every_login_names_a_known_provider_and_its_key() {
        for login in LOGINS {
            let provider = provider(login.provider).unwrap();
            assert_eq!(login.key, provider.key, "{}", login.harness);
            assert!(
                login.env.iter().any(|(_, v)| *v == Value::Key),
                "{} gets no key",
                login.harness
            );
            let pairs = LOGINS
                .iter()
                .filter(|l| l.harness == login.harness && l.provider == login.provider)
                .count();
            assert_eq!(pairs, 1, "{} {}", login.harness, login.provider);
        }
    }

    #[test]
    fn unknown_providers_and_harnesses_say_what_there_is() {
        assert!(check("claude-code", "zai").is_ok());
        assert_eq!(
            check("claude-code", "openai").unwrap_err(),
            "no provider named \"openai\"; providers are zai, gemini"
        );
        assert_eq!(
            check("claude-code", "gemini").unwrap_err(),
            "claude-code has no gemini login; it has zai"
        );
        assert_eq!(
            check("goose", "zai").unwrap_err(),
            "goose has no provider logins; harnesses with one are claude-code, qwen-code, codex, \
             pi, oh-my-pi, opencode, gemini-cli, antigravity"
        );
        assert_eq!(providers_of("opencode"), ["zai", "gemini"]);
    }

    #[test]
    fn every_fixed_value_and_model_is_one_shell_word_as_it_is() {
        let plain = |text: &str| {
            !text.is_empty()
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-:".contains(c))
        };
        for login in LOGINS {
            for (name, value) in login.env {
                if let Value::Text(text) = value {
                    assert!(plain(text), "{} {name}", login.harness);
                }
            }
            if let Some(model) = login.model {
                assert!(plain(model), "{} {model}", login.harness);
            }
        }
    }

    #[test]
    fn the_export_line_reads_the_key_from_its_file() {
        let claude = find("claude-code", "zai").unwrap();
        assert_eq!(
            exports(claude, "$(cat ~/.config/branchyard/secrets/ZAI_API_KEY)"),
            "export ANTHROPIC_AUTH_TOKEN=\"$(cat ~/.config/branchyard/secrets/ZAI_API_KEY)\" \
             ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic ENABLE_TOOL_SEARCH=false \
             ANTHROPIC_MODEL=glm-4.6"
        );
        let pi = find("pi", "zai").unwrap();
        assert_eq!(exports(pi, "$Z_AI"), "export ZAI_API_KEY=\"$Z_AI\"");
        let qwen = find("qwen-code", "zai").unwrap();
        assert_eq!(
            exports(qwen, "$Z_AI"),
            "export OPENAI_API_KEY=\"$Z_AI\" \
             OPENAI_BASE_URL=https://api.z.ai/api/coding/paas/v4 OPENAI_MODEL=glm-4.5-air"
        );
    }

    #[test]
    fn a_login_replaces_the_credentials_it_sets_aside_and_keeps_the_rest() {
        let mut context = Context::new("claude-code", Protocol::ClaudeStreamJson, "/h", "/w");
        context.private_home = true;
        context.login = Some("zai".into());
        context.secrets = vec![
            Secret::new("ZAI_API_KEY", "z"),
            Secret::new("ANTHROPIC_API_KEY", "a"),
            Secret::new("GH_TOKEN", "g"),
        ];
        let login = active(&context).unwrap();
        let derived = derive(&context, login, &["ANTHROPIC_API_KEY".into()]).unwrap();
        let names: Vec<&str> = derived.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["GH_TOKEN", "ANTHROPIC_AUTH_TOKEN"]);
        assert_eq!(derived.secret("ANTHROPIC_AUTH_TOKEN"), Some("z"));
        assert_eq!(derived.model.as_deref(), Some("glm-4.6"));
        // A model the run names wins.
        context.model = Some("glm-5.3".into());
        let derived = derive(&context, login, &[]).unwrap();
        assert_eq!(derived.model.as_deref(), Some("glm-5.3"));
        // No key, no login.
        context.secrets.retain(|s| s.name != "ZAI_API_KEY");
        assert!(derive(&context, login, &[])
            .unwrap_err()
            .contains("by harnesses login claude-code --provider zai"));
    }

    #[test]
    fn the_gateway_and_an_explicit_auth_method_turn_a_login_off() {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.login = Some("zai".into());
        assert!(active(&context).is_some());
        context.auth = Some("api-key".into());
        assert!(active(&context).is_none());
        context.auth = None;
        context.model_gateway = Some(crate::models::ModelGateway {
            url: "http://127.0.0.1:1".into(),
            anthropic: false,
            openai: true,
        });
        assert!(active(&context).is_none());
        context.model_gateway = None;
        context.login = Some("gemini".into());
        assert!(active(&context).is_none());
    }

    #[test]
    fn the_codex_config_edit_selects_the_provider_and_the_model() {
        let codex = find("codex", "zai").unwrap();
        let (file, edit) = config_edit(codex).unwrap();
        assert_eq!(file, ".codex/config.toml");
        let out = crate::edit::apply_all(&[edit], Some("approval_policy = \"untrusted\"\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            "approval_policy = \"untrusted\"\nmodel = \"glm-4.6\"\nmodel_provider = \"zai\"\n\n\
             [model_providers.zai]\nname = \"Z.ai\"\nbase_url = \"https://api.z.ai/api/v1\"\n\
             env_key = \"ZAI_API_KEY\"\nwire_api = \"responses\"\n"
        );
        assert!(config_edit(find("claude-code", "zai").unwrap()).is_none());
    }
}
