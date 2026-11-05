# What will bite you about macOS microphone approval?

macOS checks the responsible process. Terminal holds its own microphone grant;
ssh and tmux started from ssh normally inherit sshd-session's identity.
PAN-264 gives live capture launches their own identity with a disclaimed spawn.
The implementation is unreleased and the Mac hardware matrix has not run.

The child needs both SETSIGDEF and SETSIGMASK. Ignored dispositions survive
exec, and the parent blocks signals around spawn. Without the attributes the
prototype child had SigBlk 0000000000004007 and ignored SIGPIPE. With both it
had SigBlk 0000000000000000 and none of INT/QUIT/TERM/HUP/PIPE ignored.

Cargo's rustc-link-arg-bin reaches the binary's unit-test harness too, so the
macOS embedded-identity test reads the real Mach-O plist. It is not a mocked
plist query.

Ad hoc signatures mean per-binary grants. A rebuild or upgrade may require a
new approval. See the RUNBOOK for a stale grant reset.

`--no-audio`, `--wav` and `--replay` never relaunch.
A SIGINT to the thin parent's pid alone is ignored. Ctrl-C reaches the child
through their shared process group; TERM to the parent is forwarded.
SIGKILL to the parent leaves the child running outside launchd.

See [the platform decision](../../docs/DECISIONS/config-and-platform.md#macos-microphone-identity-embedded-infoplist-and-responsibility-relaunch-pan-264-2026-11-04).
