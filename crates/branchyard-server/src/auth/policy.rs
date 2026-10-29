//! Current issuer policy and exact captured-credential revalidation.

use crate::api::{App, Caller};
use crate::companion::store::PairedToken;
use crate::companion::Verified;
use crate::config::{check_principal, AuthenticationOrigin, Config, Principal};
use crate::error::ApiError;

/// Recheck the original identity's current policy without changing leaf grants.
pub(crate) fn origin_allowed(config: &Config, principal: &Principal) -> bool {
    if check_principal(principal).is_err() {
        return false;
    }
    let Some(origin) = principal.root_origin() else {
        return false;
    };
    match origin {
        AuthenticationOrigin::GoogleIdToken { .. } => config
            .google_identity
            .as_ref()
            .is_some_and(|google| google.still_allows(principal)),
        AuthenticationOrigin::Configured => {
            let subject = principal.connector_subject();
            let allows = |current: &Principal| {
                current.origin == AuthenticationOrigin::Configured
                    && current.connector_subject() == subject
                    && principal.within_policy(current)
            };
            config
                .tokens
                .iter()
                .any(|token| allows(&config.principal_for(&token.name)))
                || config
                    .credentials
                    .iter()
                    .any(|credential| allows(&credential.principal))
        }
        AuthenticationOrigin::Paired { .. } => false,
    }
}

/// Validate a stored leaf while preserving locally-issued legacy operator links.
pub(crate) fn paired_allowed(config: &Config, token: &PairedToken, now_ms: u64) -> bool {
    check_principal(&token.principal).is_ok()
        && token.usable_at(now_ms)
        && (token.principal.origin == AuthenticationOrigin::Configured
            || origin_allowed(config, &token.principal))
}

fn captured_allowed(
    config: &Config,
    principal: &Principal,
    verified: Option<&Verified>,
    paired: Option<&PairedToken>,
    configured: Option<&Principal>,
    now_ms: u64,
) -> bool {
    match verified {
        Some(verified) if verified.expires_at_ms.is_some() => paired.is_some_and(|token| {
            verified.token_sha256 == token.token_sha256
                && verified.expires_at_ms == Some(token.expires_at_ms)
                && principal.name == token.principal.name
                && principal.origin == token.principal.origin
                && principal.within_policy(&token.principal)
                && paired_allowed(config, token, now_ms)
        }),
        Some(_) => configured.is_some_and(|current| {
            principal.name == current.name
                && principal.origin == current.origin
                && principal.within_policy(current)
                && origin_allowed(config, principal)
        }),
        // No credential metadata means trusted internal policy validation only.
        // Never search for another token with a matching principal as leaf proof.
        None => origin_allowed(config, principal),
    }
}

/// Recheck a captured caller and its exact credential before a callback acts.
/// Without `Verified`, only current identity policy is checked, not leaf validity.
pub(crate) fn recheck_caller(
    app: &App,
    caller: &Caller,
    verified: Option<&Verified>,
) -> Result<(), ApiError> {
    let paired = match verified.filter(|verified| verified.expires_at_ms.is_some()) {
        Some(verified) => {
            let Some(companion) = app.companion.as_ref() else {
                return Err(ApiError::unauthorized());
            };
            companion
                .store
                .token(&verified.token_sha256)
                .map_err(|error| {
                    ApiError::internal(format!("could not recheck paired credential: {error}"))
                })?
        }
        None => None,
    };
    let configured = verified
        .filter(|verified| verified.expires_at_ms.is_none())
        .and_then(|verified| app.credentials.principal_for_hash(&verified.token_sha256));
    if captured_allowed(
        &app.config,
        &caller.0,
        verified,
        paired.as_ref(),
        configured,
        branchyard_support::time::now_ms(),
    ) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

#[cfg(test)]
mod tests;
