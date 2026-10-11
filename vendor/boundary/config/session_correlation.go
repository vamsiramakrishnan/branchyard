package config

import (
	"fmt"
	"net/url"
	"strings"

	"github.com/coder/boundary/rulesengine"
)

// Header names and paths for session correlation.
const (
	// SessionIDHeaderName is the fixed HTTP header name boundary injects to
	// carry its session ID. Coder AI Gateway expects exactly this header name.
	SessionIDHeaderName = "X-Coder-Agent-Firewall-Session-Id"

	// SequenceNumberHeaderName is the fixed HTTP header name boundary injects
	// to carry its per-session sequence number. Coder AI Gateway expects
	// exactly this header name.
	SequenceNumberHeaderName = "X-Coder-Agent-Firewall-Sequence-Number"

	// DefaultAIGatewayPath is the current AI Gateway route prefix glob used
	// when auto-deriving an inject target from CODER_AGENT_URL.
	DefaultAIGatewayPath = "/api/v2/ai-gateway/*"

	// DefaultAIBridgePath is the backward-compatible aibridge alias route
	// prefix glob used when auto-deriving an inject target from
	// CODER_AGENT_URL. It is transitional: once all Coder deployments serve
	// the gateway exclusively at DefaultAIGatewayPath, this alias and the
	// extra inject target derived from it in DefaultInjectTargetsFromEnv
	// should be removed.
	DefaultAIBridgePath = "/api/v2/aibridge/*"

	// CoderAgentURLEnv is the environment variable set by the Coder workspace
	// agent that points to the control plane. Boundary uses it to derive a
	// default inject target when none is explicitly configured.
	CoderAgentURLEnv = "CODER_AGENT_URL"
)

// SessionCorrelationConfig holds configuration for session correlation
// header injection. When enabled, boundary injects its session ID and
// sequence number as custom headers on matching outbound requests so
// that an upstream AI Bridge can correlate the request back to the
// boundary audit event stream.
type SessionCorrelationConfig struct {
	// Enabled controls whether session correlation headers are injected.
	// Deployments without AI Bridge in front should set this to false.
	Enabled bool

	// InjectTargets is the list of raw rule specs (same syntax as --allow)
	// that should receive session correlation headers. Each string uses the
	// rulesengine "domain=... path=..." format so that inject target
	// matching is identical to allow-rule matching.
	InjectTargets []string
}

// DefaultInjectTargetsFromEnv derives inject target rule strings from the
// CODER_AGENT_URL variable in the provided environment slice. It returns nil
// if the variable is absent, empty, or not a valid URL with a host. The
// derived targets cover both the current AI Gateway route prefix
// (DefaultAIGatewayPath) and its backward-compatible aibridge alias
// (DefaultAIBridgePath) so that clients hitting either path on the
// control-plane host get correlation headers injected.
//
// The environ parameter is accepted rather than reading os.Environ directly so
// that callers (and tests) can supply an arbitrary environment.
func DefaultInjectTargetsFromEnv(environ []string) []string {
	var raw string
	for _, e := range environ {
		k, v, ok := strings.Cut(e, "=")
		if ok && k == CoderAgentURLEnv {
			raw = v
			break
		}
	}
	if raw == "" {
		return nil
	}

	u, err := url.Parse(raw)
	if err != nil || u.Host == "" {
		return nil
	}

	return []string{
		fmt.Sprintf("domain=%s path=%s", u.Hostname(), DefaultAIGatewayPath),
		fmt.Sprintf("domain=%s path=%s", u.Hostname(), DefaultAIBridgePath),
	}
}

// ValidateSessionCorrelation checks that the session correlation config
// is internally consistent. When enabled it verifies that at least one
// inject target is configured and that every target string is a valid
// rulesengine rule. It returns an error describing the first problem
// found, or nil if the config is valid.
func ValidateSessionCorrelation(cfg SessionCorrelationConfig) error {
	if !cfg.Enabled {
		return nil
	}

	if len(cfg.InjectTargets) == 0 {
		return fmt.Errorf(
			"session correlation is enabled but no inject targets are configured",
		)
	}

	// Reject empty target strings before passing to the parser.
	for _, t := range cfg.InjectTargets {
		if strings.TrimSpace(t) == "" {
			return fmt.Errorf("inject target: must not be empty")
		}
	}

	// Validate each target parses as a rulesengine rule.
	rules, err := rulesengine.ParseAllowSpecs(cfg.InjectTargets)
	if err != nil {
		return fmt.Errorf("inject target: %w", err)
	}

	// Inject targets must specify a domain; path-only rules are not
	// meaningful for header injection.
	for i, r := range rules {
		if r.HostPattern == nil {
			return fmt.Errorf("inject target %q: domain is required", cfg.InjectTargets[i])
		}
	}

	return nil
}
