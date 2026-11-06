# Security Policy

## Supported versions

Only the latest GitHub Release is supported with security fixes. Older
releases may miss EC/MSR hardening and config-integrity fixes.

| Version | Supported |
| ------- | --------- |
| latest release | ✅ |
| older releases | ❌ (please update first) |

## Reporting a vulnerability

**Do not open a public issue for a suspected vulnerability.** Use one of:

1. GitHub → Security tab → **Report a vulnerability** (private vulnerability
   reporting) on
   [Framework-Crate](https://github.com/vinctw22/Framework-Crate),
   preferred — it keeps the report private until a fix ships.
2. If private reporting is unavailable, open a normal issue **without**
   exploit details and ask for a private channel.

Please include: app version, Windows version, machine model, what you did,
what you expected, and what happened. Logs from
`%APPDATA%\framework-crate\app.log` help if they do not contain personal data.

## Scope

In scope: EC fan/thermal control writes, MSR power-limit writes, PawnIO
install/download verification, config and log file integrity (symlink and
permission handling), installer/startup-task behavior, supply-chain issues
in dependencies.

Out of scope: physical access attacks, social engineering, vulnerabilities
in Windows itself or in third-party drivers, and denial of service that
requires local administrator rights you already granted the app.

## What to expect

- Acknowledgement within 7 days.
- A fix or mitigation plan for confirmed issues, then a release and
  a public note. No bug bounty — this is an independent, unpaid project.

## Full releases and notices

Security-relevant changes are listed in the
[release notes](https://github.com/vinctw22/Framework-Crate/releases).
The [project site](https://vinctw22.github.io/Framework-Crate/)
also hosts the [Privacy Policy](https://vinctw22.github.io/Framework-Crate/privacy.html)
and [License](https://vinctw22.github.io/Framework-Crate/license.html) pages.
