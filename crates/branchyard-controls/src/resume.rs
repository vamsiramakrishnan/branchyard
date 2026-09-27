// Derived from herdrdev/herdr src/agent_resume.rs, Apache-2.0.
// Upstream revision: 5f3763dda88e0fefbce14afab7cc2c7232a5e2a8.
// Modified for Branchyard: kept only `is_official_agent_source`, the
// registry of agent/source pairs Herdr's resume recipes recognize as
// official. The rest of the upstream file (session-reference types and the
// CLI argument builder) was ported for evaluation, but no caller outside
// this module ever used it: every Branchyard driver with protocol-level
// resume builds its own launch argv from the protocol handshake, not from
// this recipe, and ACP resume goes through the ACP protocol itself, never a
// harness's native, non-ACP CLI invocation (see docs/vendoring.md, "Herdr:
// reuse resume recipes, keep observations separate"). It was removed rather
// than kept as unused code; `branchyard_controls::harness`'s
// `every_herdr_resume_source_is_registered` test still validates the
// harness registry's `herdr_resume` mappings against this function, which
// is why it remains.
// Original source and license are retained in vendor/herdr/.
// These source labels classify upstream recipes; they do NOT authenticate input.

/// Whether Herdr's own resume recipes recognize `(source, agent)` as an
/// official pairing. `branchyard_controls::harness` checks every harness
/// registry entry's `herdr_resume` mapping against this, so a mapping can
/// never claim upstream support that Herdr does not actually have.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_official_agent_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:kimi", "kimi")
            | ("herdr:omp", "omp")
            | ("herdr:mastracode", "mastracode")
            | ("herdr:pi", "pi")
            | ("herdr:hermes", "hermes")
            | ("herdr:opencode", "opencode")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:qwen", "qwen")
            | ("herdr:kilo", "kilo")
            | ("herdr:cursor", "cursor")
            | ("herdr:antigravity_cli", "agy")
            | ("herdr:grok", "grok")
            | ("herdr:letta", "letta")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_herdrs_official_pairs() {
        assert!(is_official_agent_source("herdr:claude", "claude"));
        assert!(is_official_agent_source("herdr:codex", "codex"));
        assert!(is_official_agent_source("herdr:letta", "letta"));
        assert!(!is_official_agent_source("custom:claude", "claude"));
        assert!(!is_official_agent_source("herdr:claude", "codex"));
        assert!(!is_official_agent_source("herdr:aider", "aider"));
    }
}
