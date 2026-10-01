//! `token new --link`, `token list` and `token revoke`: pairing links for
//! the web companion, made and revoked in the server's own store (its data
//! directory's `state.db`, or its database), so they work whether or not
//! the server is running and take effect on it at once.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use crate::config::{self, sha256_hex, Config, Principal};

use super::store::{CompanionStore, Pairing};

/// A token's lifetime when `--ttl` is not given.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// How long a link may be opened when `--code-ttl` is not given.
pub const DEFAULT_CODE_TTL: Duration = Duration::from_secs(10 * 60);
pub const MAX_CODE_TTL: Duration = Duration::from_secs(60 * 60);

/// `30s`, `15m`, `8h`, `7d`.
pub fn duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?} needs a unit: s, m, h or d (such as 15m)"))?;
    let (number, unit) = text.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 15m, 8h or 7d"))?;
    let seconds = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        "d" => n.saturating_mul(86_400),
        _ => return Err(format!("{text:?}: the unit is s, m, h or d")),
    };
    if seconds == 0 {
        return Err("a duration must be more than zero".into());
    }
    Ok(Duration::from_secs(seconds))
}

/// What `token new --link` asked for.
pub struct LinkRequest {
    pub name: Option<String>,
    pub tenant: String,
    pub scopes: Vec<String>,
    pub repos: Option<Vec<String>>,
    pub ttl: Option<Duration>,
    pub code_ttl: Option<Duration>,
    pub public_url: Option<String>,
    pub qr: bool,
}

/// A pairing link made.
#[derive(Debug)]
pub struct Link {
    pub url: String,
    pub name: String,
    pub expires_at_ms: u64,
    pub token_ttl: Duration,
}

/// The base URL a phone opens: `public_url`, else `http(s)://<listen>`,
/// which is refused when it names every interface.
pub fn base_url(config: &Config, public_url: Option<&str>) -> Result<String, String> {
    if let Some(url) = public_url.or(config.triggers.public_url.as_deref()) {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("{url:?} is not an http:// or https:// URL"));
        }
        return Ok(url.trim_end_matches('/').to_owned());
    }
    if config.listen.ip().is_unspecified() {
        return Err(format!(
            "the server listens on {}, which a phone cannot open; pass --public-url with \
             the address it reaches the server at",
            config.listen
        ));
    }
    let scheme = if config.tls.is_some() {
        "https"
    } else {
        "http"
    };
    Ok(format!("{scheme}://{}", config.listen))
}

/// Record a pairing code in `store` for `request` and return its link.
pub fn create(
    config: &Config,
    store: &dyn CompanionStore,
    request: &LinkRequest,
    now_ms: u64,
) -> Result<(Link, String), String> {
    let base = base_url(config, request.public_url.as_deref())?;
    let ttl = request.ttl.unwrap_or(DEFAULT_TTL);
    if ttl > MAX_TTL {
        return Err("--ttl is at most 90d".into());
    }
    let code_ttl = request.code_ttl.unwrap_or(DEFAULT_CODE_TTL);
    if code_ttl > MAX_CODE_TTL {
        return Err("--code-ttl is at most 1h".into());
    }
    for scope in &request.scopes {
        config::check_scope_name(scope)?;
    }
    config::check_tenant_name(&request.tenant)?;
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| format!("phone-{}", &branchyard_client::new_key()[..6]));
    let principal = Principal {
        name: name.clone(),
        tenant: request.tenant.clone(),
        scopes: request.scopes.iter().cloned().collect(),
        repos: request.repos.as_ref().map(|r| r.iter().cloned().collect()),
    };
    config::check_principal(&principal)?;
    if config
        .all_credentials()
        .iter()
        .any(|c| c.principal.name == name && c.principal.tenant == principal.tenant)
    {
        return Err(format!(
            "{name} names a credential in the server's configuration; choose another --name"
        ));
    }
    let code = super::random_hex(16)?;
    let expires_at_ms = now_ms + code_ttl.as_millis() as u64;
    let pairing = Pairing {
        code_sha256: sha256_hex(code.as_bytes()),
        principal,
        token_ttl_ms: ttl.as_millis() as u64,
        created_at_ms: now_ms,
        expires_at_ms,
    };
    if !store
        .create_pairing(&pairing, now_ms)
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "{name} already names a paired token or an unopened link; revoke it first \
             (token revoke {name}) or choose another --name"
        ));
    }
    let link = Link {
        // The code travels in the fragment: browsers never send it to the
        // server in the request line, so no access log can record it.
        url: format!("{base}/app/#pair={code}"),
        name,
        expires_at_ms,
        token_ttl: ttl,
    };
    Ok((link, code))
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn when(ms: u64) -> String {
    jiff::Timestamp::from_millisecond(ms as i64)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| ms.to_string())
}

/// `token new --link`: print the link on stdout and, on a terminal, a QR
/// code of it on stderr.
pub fn new_link(config: &Config, request: LinkRequest, program: &str) -> ExitCode {
    let store = match super::store::open(config) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("{program}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (link, _) = match create(config, store.as_ref(), &request, crate::ops::now_ms()) {
        Ok(made) => made,
        Err(error) => {
            eprintln!("{program}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let stderr = std::io::stderr();
    if request.qr && stderr.is_terminal() {
        match super::qr::QrCode::encode(link.url.as_bytes()) {
            Ok(code) => eprint!("{}", code.to_ansi()),
            Err(error) => eprintln!("{program}: no QR code: {error}"),
        }
    }
    println!("{}", link.url);
    eprintln!(
        "{program}: pairing link for {} (tenant {}, scopes {}): opens once, until {}; the \
         token it gives lasts {}. Revoke with: token revoke {}",
        link.name,
        request.tenant,
        request.scopes.join(","),
        when(link.expires_at_ms),
        human(link.token_ttl),
        link.name,
    );
    if !config.app.enabled {
        eprintln!(
            "{program}: the server must run with --app (or \"app\": true) for the link to open"
        );
    }
    ExitCode::SUCCESS
}

/// `token list`.
pub fn list(config: &Config, program: &str) -> ExitCode {
    let listed =
        super::store::open(config).and_then(|store| store.tokens().map_err(|e| e.to_string()));
    let (tokens, codes) = match listed {
        Ok(listed) => listed,
        Err(error) => {
            eprintln!("{program}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let now = crate::ops::now_ms();
    let mut rows = vec![[
        "NAME".to_owned(),
        "TENANT".to_owned(),
        "SCOPES".to_owned(),
        "STATE".to_owned(),
        "UNTIL".to_owned(),
        "DEVICE".to_owned(),
    ]];
    for code in &codes {
        let state = match code.expires_at_ms > now {
            true => "link not opened",
            false => "link expired",
        };
        rows.push([
            code.principal.name.clone(),
            code.principal.tenant.clone(),
            scopes(&code.principal),
            state.into(),
            when(code.expires_at_ms),
            String::new(),
        ]);
    }
    for token in &tokens {
        let state = match (token.revoked_at_ms, token.usable_at(now)) {
            (Some(_), _) => "revoked",
            (None, true) => "active",
            (None, false) => "expired",
        };
        rows.push([
            token.principal.name.clone(),
            token.principal.tenant.clone(),
            scopes(&token.principal),
            state.into(),
            when(token.revoked_at_ms.unwrap_or(token.expires_at_ms)),
            token.device.clone().unwrap_or_default(),
        ]);
    }
    if rows.len() == 1 {
        eprintln!("{program}: no paired tokens or pairing links (token new --link makes one)");
        return ExitCode::SUCCESS;
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    ExitCode::SUCCESS
}

fn scopes(p: &Principal) -> String {
    p.scopes.iter().cloned().collect::<Vec<_>>().join(",")
}

/// `token revoke NAME`.
pub fn revoke(config: &Config, name: &str, tenant: Option<&str>, program: &str) -> ExitCode {
    let revoked = super::store::open(config).and_then(|store| {
        store
            .revoke(name, tenant, crate::ops::now_ms())
            .map_err(|e| e.to_string())
    });
    match revoked {
        Ok(0) => {
            let configured = config
                .all_credentials()
                .iter()
                .any(|c| c.principal.name == name);
            match configured {
                true => eprintln!(
                    "{program}: {name} is a credential in the server's configuration: remove \
                     it there and restart the server to revoke it"
                ),
                false => {
                    eprintln!("{program}: no active paired token or pairing link named {name}")
                }
            }
            ExitCode::FAILURE
        }
        Ok(n) => {
            eprintln!(
                "{program}: revoked {name} ({n} token{} or link{}); its requests and push \
                 notifications stop now",
                if n == 1 { "" } else { "s" },
                if n == 1 { "" } else { "s" },
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{program}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_a_unit() {
        assert_eq!(duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(duration("8h").unwrap(), Duration::from_secs(8 * 3600));
        assert_eq!(duration("7d").unwrap(), Duration::from_secs(7 * 86_400));
        assert_eq!(duration("30s").unwrap(), Duration::from_secs(30));
        for bad in ["15", "m", "0m", "1w", "-1h", ""] {
            assert!(duration(bad).is_err(), "{bad}");
        }
        assert_eq!(human(Duration::from_secs(86_400)), "1d");
        assert_eq!(human(Duration::from_secs(900)), "15m");
    }

    #[test]
    fn links_carry_the_code_in_the_fragment_and_record_only_its_hash() {
        let mut config = Config::new("/tmp/unused".into());
        config.tokens.push(crate::config::Token {
            name: "ops".into(),
            secret: "0123456789abcdef".into(),
        });
        let store = super::super::store::SqliteCompanion::memory();
        let request = LinkRequest {
            name: Some("phone".into()),
            tenant: "default".into(),
            scopes: vec!["read".into()],
            repos: None,
            ttl: Some(Duration::from_secs(3600)),
            code_ttl: None,
            public_url: Some("https://by.example.com/".into()),
            qr: false,
        };
        let (link, code) = create(&config, &store, &request, 1_000).unwrap();
        assert_eq!(link.url, format!("https://by.example.com/app/#pair={code}"));
        assert_eq!(link.expires_at_ms, 1_000 + 600_000);
        let (_, codes) = store.tokens().unwrap();
        assert_eq!(codes[0].code_sha256, sha256_hex(code.as_bytes()));
        assert_ne!(codes[0].code_sha256, code);
        assert_eq!(codes[0].token_ttl_ms, 3_600_000);
        // A name in use is refused, as is a configured credential's.
        assert!(create(&config, &store, &request, 2_000)
            .unwrap_err()
            .contains("already names"));
        let ops = LinkRequest {
            name: Some("ops".into()),
            ..request
        };
        assert!(create(&config, &store, &ops, 2_000)
            .unwrap_err()
            .contains("configuration"));
        // Without a public URL, a wildcard listener has no usable link.
        config.listen = "0.0.0.0:8421".parse().unwrap();
        assert!(base_url(&config, None)
            .unwrap_err()
            .contains("--public-url"));
        config.listen = "127.0.0.1:8421".parse().unwrap();
        assert_eq!(base_url(&config, None).unwrap(), "http://127.0.0.1:8421");
    }
}
