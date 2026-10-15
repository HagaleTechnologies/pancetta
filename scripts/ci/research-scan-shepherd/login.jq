# GraphQL drops a Bot's `[bot]` suffix that REST, webhooks and `actors` carry
# (wait-for-codex.yml). Restore it from __typename so snapshots and live re-reads
# compare the REST form. A User's login, and a missing author, pass through as is.
def login: if .__typename == "Bot" then .login + "[bot]" else .login end;
