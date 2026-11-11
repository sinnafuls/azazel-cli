---
name: security-review
description: >-
  Use to look for security defects in code you are allowed to assess: a change before merge, a feature that handles untrusted input, or a whole service. Covers injection, authorisation, secrets, deserialisation, request forgery to internal services, and dependency risk, with a findings format that requires evidence.
---
# Security review

Find the places where someone other than the intended user can make the program do something its owner did not intend, and prove each one from the code. A finding without a concrete path from attacker-controlled input to harmful effect is a suspicion; report it as a question, not a vulnerability.

This is defensive review of the user's own code. Do not test against systems the user does not operate, and do not write working exploits beyond the minimal demonstration needed to confirm a finding locally.

## 1. Map the attack surface

Before reading for bugs, list:

- **Entry points**: HTTP routes, RPC methods, message consumers, CLI arguments, file parsers, webhooks, scheduled jobs that read external data.
- **Trust boundaries**: where data crosses from a less trusted party to a more trusted one (browser to server, tenant to tenant, user to administrator, network to host, plugin to core).
- **Assets**: what an attacker wants: credentials, personal data, money movement, code execution, other tenants' data.
- **Authentication and session mechanism**, and where authorisation decisions are made.

Use `grep` for route registrations, handler attributes and deserialisation calls to build the list. For a diff review, limit the map to what the change touches plus what it can reach.

## 2. Trace input to sinks

For each entry point, follow the attacker-controlled values to where they are used. The classes to check:

### Injection
| Sink | Look for | Safe form |
|---|---|---|
| SQL and other query languages | String concatenation or formatting into a query | Parameterised queries; allow-listed identifiers for dynamic table or column names |
| Shell | Input in a command string; `shell=True`; `system`, backticks | Argument arrays without a shell; fixed commands |
| File paths | Input joined into a path; archive extraction | Resolve, then check the result is inside the intended root; reject `..`, absolute paths and symlinks out |
| HTML and templates | Unescaped output; `innerHTML`; "safe" or "raw" filters; template source built from input | Contextual auto-escaping; a sanitiser for rich text |
| Code evaluation | `eval`, dynamic import or reflection on input | Remove; map input to a fixed set of functions |
| Headers, logs, emails | Newlines in input | Strip or encode control characters |
| LDAP, XPath, regular expressions | Input in the expression; catastrophic backtracking | Escape for the language; bounded patterns or a linear-time engine |

### Authentication and authorisation
- Every entry point checks who the caller is, unless it is public on purpose.
- Authorisation is checked on the object being acted on, not just on the route: can user A pass user B's identifier? Look for lookups by id with no owner or tenant condition.
- Checks happen on the server. A hidden button is not a control.
- Privilege changes, password and email changes, and destructive actions require fresh authentication or confirmation.
- Tokens: signed and verified with a fixed algorithm, expiry enforced, audience and issuer checked, revocable. Session cookies are `HttpOnly`, `Secure`, with a `SameSite` policy; state-changing requests are protected against cross-site forgery.
- Password storage uses a slow, salted algorithm (argon2id, scrypt, bcrypt). Comparisons of secrets are constant time.
- Rate limits on login, reset and anything that sends messages or costs money.

### Secrets
- No keys, passwords or tokens in source, fixtures, container images or client-side bundles. Search for high-entropy strings and for names like `key`, `secret`, `token`, `password`.
- Secrets do not appear in logs, error messages, URLs or analytics.
- Check history when a secret was found in the working tree: if it was ever committed, it must be rotated, not just removed.
- Encryption uses a vetted library with authenticated modes; keys and nonces come from a cryptographic random source; nothing invents its own scheme.

### Deserialisation and parsing
- Formats that can instantiate arbitrary types (native object serialisation, YAML with unsafe loaders, XML with external entities) must never receive untrusted data in that mode.
- Parsers of uploaded files run with size, depth and time limits. Decompression is bounded.
- Mass assignment: request bodies bound directly onto models can set fields the user should not control (`role`, `owner_id`, `verified`).

### Requests made on the user's behalf
- Any feature that fetches a URL supplied by a user (webhooks, importers, previews, image proxies) can be pointed at internal services and cloud metadata endpoints.
- Required: allow-list of schemes, resolution of the host followed by a check of the resolved address against private, loopback and link-local ranges, the same check after every redirect, and timeouts and size limits.
- Open redirects: redirect targets taken from input must be relative or allow-listed.

### Dependencies and supply chain
- Run the ecosystem's audit (`cargo audit`, `npm audit`, `pip-audit`, `govulncheck`) when available and report what it finds for the versions in the lockfile.
- New dependencies in the change: who publishes them, how widely used, what install scripts they run, whether the name is a near-miss of a popular package.
- Lockfiles are committed; versions are pinned in build and CI configuration; CI does not expose secrets to code from forks.

### Everything else worth a look
- Error handling that fails open: an exception in a permission check that results in access.
- Race conditions between a check and the use of its result, especially on files and balances.
- Verbose errors and debug endpoints reachable in production configuration.
- Permissive cross-origin policy combined with credentials.
- Uploads: type checked by content, stored outside the web root, served with a safe content type.
- Memory safety in native code: lengths taken from input, integer overflow in size computations, unchecked indexing.

## 3. Confirm

For each candidate, write the path: entry point, the tainted value, each function it passes through, the sink, and why no check stops it. Read the checks that exist carefully; many candidates die here. Where it is safe and local, confirm with a test or a request against a development instance.

## 4. Severity

| Level | Meaning |
|---|---|
| `critical` | Unauthenticated remote code execution, authentication bypass, or bulk access to other users' data |
| `high` | The same with a low-privilege account; injection with real impact; exposed live secrets |
| `medium` | Requires unusual conditions or user interaction; limited data exposure; missing hardening that enables other attacks |
| `low` | Defence-in-depth gaps with no direct impact |
| `info` | Observations and good practices |

Rate by what an attacker gains and what they need to already have.

## 5. Findings format

```
# Security review: <scope, commit>

Summary: <counts by severity; one sentence on the most important issue>

## Findings

### 1. <title> (high)
Location: path/to/file.ext:line
Path: <entry point> -> <function> -> <sink>
What is wrong: <the missing or faulty control>
Impact: <what an attacker can do, and what they need first>
Evidence: <the code lines; the request or test that demonstrates it, if run>
Fix: <the specific change; where the same pattern also occurs>

## Checked and found sound
<areas reviewed with no findings, so the reader knows the coverage>

## Not reviewed
<what was out of scope or could not be assessed>
```

Report live secrets to the user immediately and do not repeat their values in the report; give the location only.
