# Security policy

Groovy moves private files and messages between devices, so please report security problems privately.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**. Only the maintainers can see the report.

Please include what you found, how to reproduce it, and which version of Groovy you used. Don't open a public issue for a vulnerability.

Problems in iroh, the transport Groovy is built on, should go to the iroh maintainers, n0 computer, through their own channels.

## What to expect

We aim to reply within a week. Once a fix is ready, it ships in a release, and the advisory is published with credit to the reporter if they want it.

## Scope

The [threat model](docs/threat-model.md) says what Groovy is meant to protect against and what it does not. A report that a documented limit exists is not a vulnerability, but a report that the limit is wrong is welcome.
