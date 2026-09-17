# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's **Report a vulnerability**
button on this repository (Security → Advisories). Do not open a public issue.

Include what you found, how to reproduce it, and the impact you expect. You will get an
acknowledgement within a few days.

## Scope

The following are in scope:
- anything that lets an attacker run code, inject content into pages, bypass the denylist or the CSAM filter, exhaust resources with small inputs, or learn peer or visitor IP addresses;
- the DHT and peer-wire parsers;
- the web front end;
- the database access layer;
- the container and compose configuration.

Reports about content that appears in an instance's index (copyright, abuse, CSAM)
should go to **that instance's operator**, using its `/legal` page or report form. They
should not come here.

## Design notes

The security design and the reasons behind it are in
[docs/01-first-principles.md](docs/01-first-principles.md) and
[docs/03-design.md](docs/03-design.md). Hard limits are listed in §3 of the design.
