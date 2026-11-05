# Contest scoring

PAN-50 supports the 2026 Kansas FT4/FT8 and South Carolina fixed digital
rules. These are local claimed scores before sponsor adjudication. Scoring
identity is separate from the built-in family profile's exchange verification.
Only Kansas scoring enables its grid acknowledgement matcher. SC contacts
carry scoring identity without enabling that matcher.

Kansas counts unique eligible contacts without multipliers. Duplicate keys
include normalized callsign, band, mode and explicit Kansas county where
available. If county evidence is missing in a group, collapse that group
conservatively to one contact. Portable callsign suffixes remain significant.
SC combines digital modes for duplicates and scopes multipliers by band.
Outside entrants score SC counties only. SC entrants count SC counties,
US states including SC and DC, and Canadian provinces/territories. DX earns
points without multipliers. SC partners earn two points; other eligible
partners earn four for SC entrants. Final score is points times multipliers
plus W4CAE 350, WW4SF 250 and K4YTZ 250, once per band/digital group.

Sources are the [2026 Kansas rules, item 16](https://www.ksqsoparty.org/rules/KSQPRules2026.pdf)
and [SC revision 2.2.26, sections 9.1 through 9.4](https://scqso.com/wp-content/uploads/2026/02/SCQPRULES2024_0126.pdf).
County input currently accepts short alphanumeric codes, not a full sponsor
county vocabulary. Operators must supply sponsor county abbreviations.
New editions require new pinned rules and fixtures.

New manual attempts starting within half-open UTC periods store four local
ADIF fields, without inventing a standard `CONTEST_ID` for the family:
`APP_PANCETTA_CONTEST_INSTANCE`, `APP_PANCETTA_CONTEST_RULES`,
`APP_PANCETTA_CONTEST_CONTEXT`, `APP_PANCETTA_CONTEST_CONTACT_ID`.
The last field is a stable UUID independent of rebuilt database row UUIDs.
Context version 1 stores profile, operator callsign, operator region, periods
and nullable explicit partner region/county, entrant county and source.
An optional `contact_start` preserves a live contact's fractional seconds,
which standard ADIF TIME_ON cannot carry. Its whole second must match the
standard QSO start; older annotated imports use the standard timestamp.
Unknown versions and malformed contexts remain verbatim on export and make
relevant scoring unavailable. Conflicting location evidence stays unresolved;
raw fields remain available for correction and reimport. Ordinary records
receive no application fields. No old log is retrospectively enrolled.

The strict query selects completed records by instance and actual start time,
without ordinary search pagination. It rejects unreadable rows and conflicting
instance settings before period filtering. The runtime reader checks this
index against the ADIF source of truth. Pending or failed writes hide numeric
scores. It retries failed reads, refreshes on file changes and pending writes,
and reconciles every 30 seconds. One sequential worker per QSO component owns
refreshes, exits on shutdown and is aborted on component teardown. Metrics
use one rule context and generation. Headless mode uses the same worker.
No history I/O holds a QSO lock or runs on the TX timing path.

Future contacts earn no credit. Rate counts scored starts in the trailing
wall-clock hour, decays on ticks and freezes at final period end. The typed
strip sits below readiness, survives diagnostic/status/decode traffic, ignores
older generations and uses one row below 60 columns. History failures replace
numbers until recovery. Disabled scoring consumes no rows.
