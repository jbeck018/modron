---
name: security-review
description: OWASP Top 10 audit with secrets detection, input-validation sweep, severity taxonomy, and remediation priorities. Use before shipping anything that touches user input, auth, or network.
---

# Security Review

Adapted from the OMX/jcode security-review skill; OWASP-anchored.

## Scope sweep

1. **Secrets**: scan for hardcoded keys/tokens/passwords, secrets in
   logs, world-readable credential files, keys in argv. (Rotate any found
   within the hour — exposure, not discovery, is the deadline.)
2. **Input validation** (A03): every boundary that accepts external data
   — HTTP params, file paths from the model, env vars, CLI args — is
   either validated, escaped, or capability-fenced. Flag path traversal,
   injection into `sh -c`, and SSRF-able URLs.
3. **Authn/authz** (A01/A07): missing checks on sensitive paths, session
   fixation, tokens that never expire.
4. **Crypto** (A02): hardcoded IVs, homebrew hashing, ECB, unverified
   TLS.
5. **Dependencies** (A06): run the project's audit (`cargo audit`,
   `npm audit`) and list anything critical.
6. **Logging** (A09): sensitive data written to logs or transcripts.

## Severity definitions

- **CRITICAL**: remotely exploitable, auth bypass, secret disclosure.
  Fix before merge; rotate exposed secrets immediately.
- **HIGH**: exploitable with prerequisites (local access, insider).
  Fix within 24h.
- **MEDIUM**: defense-in-depth gap, hardening missing.
- **LOW**: informational, hygiene.

## Report format

One finding per item — `[SEVERITY] file:line — title. Impact: one
sentence. Remediation: one sentence. (OWASP Axx)` — then a final
posture verdict: SHIP / FIX-FIRST / DO-NOT-SHIP.

## Rules

- Every finding cites a file:line you actually read.
- For an independent second opinion, spawn a fresh `delegate` with this
  same skill on the riskiest files — do not consult external services.
- Recommend fixes, do not silently apply them (security changes get
  reviewed, not smuggled).
