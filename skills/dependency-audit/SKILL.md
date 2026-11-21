---
name: dependency-audit
description: "Use to check a project's dependencies, runtimes and base images for known vulnerabilities and end-of-life versions. Reads lockfiles, runs osv-scanner or trivy when installed (else queries OSV), checks CISA KEV and endoflife.date, and reports what to upgrade, with the command. Read-only."
argument-hint: "[path or focus]"
---
# Dependency audit

Find out which parts of this project's stack have known vulnerabilities or no longer get fixes, and say what to upgrade and how. `$ARGUMENTS` may name a directory (`web/`) or a focus (`the Python service`); with no arguments, cover the whole repository. This is read-only: change no file, install nothing and upgrade nothing. The user asks for that afterwards.

Every claim in the report must come from a file you read, a tool you ran or one of the sources below. Say so when something is from memory or unverified; never present a guessed CVE as a finding.

## 1. Inventory

List the stack from what is on disk, not from what you remember. Look in the scope for:

- **Lockfiles (exact versions):** `Cargo.lock`, `package-lock.json`, `bun.lock`, `pnpm-lock.yaml`, `yarn.lock`, `poetry.lock`, `uv.lock`, `Pipfile.lock`, `go.sum`, `Gemfile.lock`, `composer.lock`, `gradle.lockfile`, `packages.lock.json`.
- **Manifests without a lockfile:** `requirements*.txt`, `go.mod`, `pom.xml`, `*.csproj`. A range such as `^1.2` is not a version: resolve it from the lockfile, or mark the component "version not pinned".
- **Toolchains and runtimes:** `rust-toolchain(.toml)`, `.nvmrc`, `.node-version`, `.python-version`, `.tool-versions`, `engines` in `package.json`, `go` and `toolchain` lines in `go.mod`, `rust-version` in `Cargo.toml`.
- **Containers and CI:** `FROM` lines of every `Dockerfile` and compose file, and the `uses:` pins and runner images in `.github/workflows/`. A pin to a tag or branch (`@v4`, `@main`) is worth noting; a full commit hash is exact.

Use `glob` to find the files and read the lockfiles with `grep` for the names you need rather than in full when they are large. Skip `node_modules/`, `target/`, `vendor/` and build output.

Record each component as: ecosystem, name, exact resolved version, the file it came from, and **direct** or **transitive** (a direct dependency is named in a manifest; the rest are transitive). When a lockfile cannot tell you which, say "unknown".

## 2. Scan

1. Check which scanners are installed: `osv-scanner --version` and `trivy --version`. Run those that exist, read-only and offline-safe options first:
   - `osv-scanner scan source -r <path>` (older versions: `osv-scanner -r <path>`), or `osv-scanner --lockfile <file>` for one lockfile;
   - `trivy fs --scanners vuln <path>`.
   Say which ran and their versions. Do not install a scanner; if neither is present, say so and offer to explain how to install one.
2. When a scanner is missing, or for components it did not cover (Dockerfile bases, toolchains), query OSV directly with the `http` tool or a short `eval` script:
   - `POST https://api.osv.dev/v1/querybatch` with `{"queries": [{"package": {"ecosystem": "crates.io", "name": "<name>"}, "version": "<version>"}, ...]}`. Ecosystem names are OSV's: `crates.io`, `npm`, `PyPI`, `Go`, `RubyGems`, `Packagist`, `Maven`, `NuGet`. The batch holds at most 1000 queries; split larger inventories. A result with no `vulns` means none known.
   - `GET https://api.osv.dev/v1/vulns/<id>` for each id returned, to read its summary, severity, aliases, `affected[].ranges` and the `fixed` event.
   Do not send anything but package names and versions.
3. If a source cannot be reached, report that as not checked. Do not fill the gap from memory.

## 3. Describe each finding

For every vulnerable component give:

- the ids: OSV, GHSA and CVE (from `aliases`);
- severity: the CVSS score and vector when present, else the advisory's own rating, else "not rated";
- the affected range and the **first fixed version** that is above the installed one (the first fixed version in the installed major line when there is one, and the next major when that is the only fix); "no fix released" when there is none;
- **KEV:** whether any of its CVEs is in the CISA Known Exploited Vulnerabilities catalogue, `https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json` (fetch it once, then search for the CVE ids in `vulnerabilities[].cveID`; a large file is best searched with `web_grep` or a script, not read in full);
- **reachability:** a judgement from the code, labelled as one. Say whether the dependency is direct or transitive, runtime or dev/build only, and whether the vulnerable function or feature named in the advisory is used (search for the symbol). Use "reachable", "plausibly reachable", "not reachable" or "unknown" and give the reason in a few words. Do not call something unreachable without having looked.

## 4. End of life

For each runtime, toolchain, base image and major framework in the inventory (for example `nodejs`, `python`, `go`, `ubuntu`, `debian`, `alpine`, `postgresql`, `django`, `rails`, `angular`), fetch `https://endoflife.date/api/<product>.json`. The response is a list of cycles with `cycle`, `latest`, `eol` (a date, or `false`), and sometimes `support` and `lts`. Report the installed version, its cycle's latest version, and its EOL date. A product the API does not know (a 404) is "not checked", not "fine".

## 5. Report

Compare dates with today's date, not with your training data. Group findings:

- **Upgrade now:** a CVE in the KEV catalogue; a critical or high finding that is reachable or plausibly reachable; a runtime or base image that is already end of life.
- **Upgrade soon:** medium findings; high or critical findings of unknown reachability; anything whose EOL is within six months.
- **No action:** no fix released; not reachable; dev-only and low. Say why for each, so the user can disagree.

Under each item give the component and installed version, the ids and severity, KEV yes/no, reachability, **the exact version to move to**, and the command that does it, using the project's own tooling. Examples:

- Cargo: `cargo update -p <crate> --precise <version>`, or edit the requirement in `Cargo.toml` when the fix is a new major.
- Bun, npm, pnpm, yarn: `bun update <pkg>@<version>`, `npm install <pkg>@<version>`, `pnpm update <pkg>@<version>`, `yarn up <pkg>@<version>`. For a transitive package, name the direct dependency to raise, or an `overrides` / `resolutions` entry.
- Python: `uv lock --upgrade-package <pkg>==<version>`, `poetry update <pkg>`, or the line to change in `requirements.txt`.
- Go: `go get <module>@<version>` then `go mod tidy`. Ruby: `bundle update <gem>`. PHP: `composer update <package>`.
- Images and runtimes: the new `FROM` line, or the version to put in `.nvmrc` or `rust-toolchain.toml`.

Do not run these commands. If a fix has a breaking major version, say that.

Open with a one-line count (for example "3 to upgrade now, 2 soon, 4 no action; 212 components inventoried"), then the groups, then the inventory in a compact form (counts per ecosystem and lockfile; the full list only if the user asks).

## 6. End with what was not checked

Always finish with a **Not checked** list: ecosystems or files you found no scanner or API for; components whose version was not pinned; sources you could not reach; vendored or copied code; system packages inside images (a Dockerfile's `apt install` is not covered by a lockfile scan unless `trivy` scanned the image); advisories newer than the databases you queried; and that reachability is a reading of the code, not a proof.
