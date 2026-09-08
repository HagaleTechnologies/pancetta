# Security review scope — 2026-09-07

Review baseline: `17ffe36` (Pancetta 0.9.6).

The application-security review covered runtime trust boundaries,
remote gateway and station-agent authorization, radio transmission controls,
network and file parsers, credentials and privacy, dependencies, packaging,
CI/CD, and security documentation. Existing reviews were checked against
the reviewed implementation. External services and sibling applications are context,
not independently assessed deployments.

Detailed findings and validation evidence remain local for owner review, in
accordance with `SECURITY.md`. This document claims the review and does not
publish vulnerability details or assert that the implementation is secure.

The Markdown report and reproducible local evidence are complete. Production
source is unchanged. The shipped-crate test run (excluding pancetta-research)
passed; the required full-workspace run failed in the research eval test's
curated baseline fixture preflight (missing `freq_hz`). The report records both
results and the review's coverage limits. Remediation is separate work.
