# Security review scope — 2026-09-07

Review baseline: `17ffe36` (Pancetta 0.9.6).

An application-security review is in progress covering runtime trust boundaries,
remote gateway and station-agent authorization, radio transmission controls,
network and file parsers, credentials and privacy, dependencies, packaging,
CI/CD, and security documentation. Existing reviews will be checked against
current implementation. External services and sibling applications are context,
not independently assessed deployments.

Detailed findings and validation evidence remain local for owner review, in
accordance with `SECURITY.md`. This document claims the review and does not
publish vulnerability details or assert that the implementation is secure.
