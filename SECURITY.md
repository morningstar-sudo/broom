# Security Policy

## Supported versions

Broom ships as rolling releases from `main` (tags `v<version>-<short-sha>`). Security fixes go into the **latest
release** only; please update before reporting, and check whether the issue still happens there.

| Version | Supported |
|---|---|
| latest release | ✅ |
| anything older | ❌ — update to the latest release |

## Reporting a vulnerability

**Please do not open a public issue, discussion or pull request for a security problem.**

Report it privately through GitHub: repository → **Security** → **Report a vulnerability**
([private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)).
Include:

- the affected version and component (web/API, iSCSI daemon, DHCP/TFTP, Windows stage, prep / client scripts);
- how to reproduce it, and what an attacker gains (from where: the boot LAN, the internet, a client machine);
- a proof of concept if you have one — redact real keys, passwords and addresses.

What to expect:

- an acknowledgement within **7 days**;
- an assessment and, for a confirmed issue, a fix plan within **30 days** — sooner for anything remotely exploitable;
- a GitHub security advisory published together with the fixed release, crediting you unless you prefer otherwise.

Please give us reasonable time to release a fix before disclosing details publicly.

## Scope and known limits

In scope: anything that lets someone **bypass the admin login**, run code on the server, read or change data they
shouldn't (license keys, the guest password, other machines' sessions), break the reset of a client between
sessions, or take the server down from a client machine or the LAN with little effort.

Some properties come with diskless boot on a shared LAN and are documented, not bugs (README → **Security**):

- the iSCSI portal, TFTP and the goldens over HTTP are readable by any host on the boot LAN — that is how clients
  boot; the boot LAN must be segmented from guest Wi-Fi and unknown devices;
- license keys are handed out once over plain HTTP, to a registered machine identified by its fixed IP + ARP;
- games disk writes are accepted from the update machine's IP only (iSCSI carries no other proof of identity);
- the guest account is a local administrator on Windows clients (games need it): a determined guest can interfere
  with the machine they sit at until its next reset.

Reports that show one of these is weaker than documented (e.g. reachable from outside the boot LAN, or affecting
other machines) are in scope.
