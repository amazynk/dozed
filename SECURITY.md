# Security policy

dozed stops and restarts other people's programs, so a way to make it act on the wrong
process, or for the wrong person, matters to us. Thank you for reporting such things
responsibly.

## Reporting a vulnerability

Please **do not open a public issue**. Report it privately through
[GitHub's private vulnerability reporting](https://github.com/amazynk/dozed/security/advisories/new),
or by email to [help@amazynk.com](mailto:help@amazynk.com) if you can't use GitHub.

Include the version, your distribution and desktop session, what an attacker could achieve,
and the steps that reproduce it. We will acknowledge the report within a few days, keep you
updated while we fix it, and credit you in the advisory unless you prefer otherwise.

## Scope

In scope: another user or another machine getting dozed to freeze, thaw or throttle anything;
dozed acting on a process its own user does not own; a client or a compositor helper being
accepted by the daemon when it should not be; the installer writing outside `$HOME`; crashes
or hangs from a crafted config, trace or window report; and anything that makes dozed send
or receive data over the network. It has no network code of its own: what it opens is the
session bus, its own socket and the display server you are already using.

Out of scope: problems that need an attacker who already runs code as your user, who can by
definition stop your processes without dozed, and the RAPL permission described in
[docs/cpu-power.md](docs/cpu-power.md), which is a tradeoff you opt into yourself.

## Supported versions

Until 1.0, only the latest release gets security fixes.
