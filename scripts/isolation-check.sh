#!/usr/bin/env bash
#
# isolation-check.sh — verify that `hako run` is a real security boundary.
#
# Exercises a running container and asserts the properties a production
# container runtime must provide: a private PID view, no host $HOME, a private
# /tmp, and network isolation by default. Run on Linux (native or WSL2).
#
#   HAKO=/path/to/hako ./scripts/isolation-check.sh
#
# Exits non-zero if any property is violated.

set -u
HAKO="${HAKO:-hako}"
# Resolve a relative path (e.g. target/debug/hako) to absolute BEFORE we cd into
# a temp dir below — otherwise every hako call silently fails and the absence
# checks become false passes. A bare command name (on PATH) is left as-is.
case "$HAKO" in
  */*) HAKO="$(cd "$(dirname "$HAKO")" && pwd)/$(basename "$HAKO")" ;;
esac
# `hako run` takes a BRANCH name; a fresh `hako init` container's branch is `main`.
BRANCH="${HAKO_BRANCH:-main}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

fail=0
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; fail=1; }
check() { if eval "$2"; then pass "$1"; else bad "$1 (got: ${3:-?})"; fi; }

echo "hako isolation check  (binary: $HAKO)"
if ! "$HAKO" init >/dev/null 2>&1; then
  echo "FATAL: '$HAKO init' failed — binary not found or not runnable" >&2
  exit 2
fi

# HAKO_RUN_FLAGS lets callers pass extra `run` flags (e.g. --no-workspace).
# Use an ABSOLUTE /bin/sh: bare `sh` relies on the container PATH resolving it,
# which isn't guaranteed across environments (e.g. CI runners) and would make
# the run produce no output — silently turning absence checks into false passes.
run() { "$HAKO" run ${HAKO_RUN_FLAGS:-} "$BRANCH" /bin/sh -c "$1" 2>/dev/null; }

# Preflight: the container must actually run, or the absence checks below would
# pass vacuously. Fail loudly if a trivial command doesn't round-trip.
if [ "$(run 'echo HAKO_RUNS')" != "HAKO_RUNS" ]; then
  echo "FATAL: container did not run (\`hako run\` produced no output)" >&2
  exit 2
fi

# 1. PID namespace — the container must NOT see host processes. With a private
#    PID namespace the highest visible pid is tiny (its own pid 1 + the probe).
maxpid="$(run 'ls /proc | grep -E "^[0-9]+$" | sort -n | tail -1')"
check "PID namespace isolates host processes" '[ -n "$maxpid" ] && [ "$maxpid" -lt 100 ]' "$maxpid"

# 2. Host $HOME must not be mounted into the container.
sentinel="$HOME/.hako_iso_home_$$"; echo secret >"$sentinel"
seen="$(run 'cat /root/.hako_iso_home_* /home/*/.hako_iso_home_* 2>/dev/null')"
check "host \$HOME is not exposed" '[ -z "$seen" ]' "$seen"
rm -f "$sentinel"

# 3. /tmp must be private (host /tmp not shared into the container).
htmp="/tmp/.hako_iso_tmp_$$"; echo secret >"$htmp"
seen="$(run 'cat /tmp/.hako_iso_tmp_* 2>/dev/null')"
check "/tmp is private to the container" '[ -z "$seen" ]' "$seen"
rm -f "$htmp"

# 4. Network isolation by default — a fresh net namespace has only loopback and
#    no default route. /proc/net/route holds just its header line when isolated.
routelines="$(run 'cat /proc/net/route 2>/dev/null | wc -l')"
check "network is isolated by default (no host routes)" '[ -n "$routelines" ] && [ "$routelines" -le 1 ]' "$routelines"

# 5. Seccomp — a blocked syscall must fail. `mount` is on the denylist, so a
#    tmpfs mount (which a userns-root workload could otherwise do) returns EPERM.
#    With the filter active the applet prints "Operation not permitted".
mnt="$(run 'mount -t tmpfs none /mnt 2>&1; echo rc=$?')"
check "seccomp blocks the mount syscall" 'echo "$mnt" | grep -q "rc=0" && false || echo "$mnt" | grep -qi "not permitted"' "$mnt"

# 6. /sys is read-only — writes must fail (fresh RO sysfs for the isolated-net run).
sysrc="$(run 'touch /sys/.hako_iso_w 2>&1; echo rc=$?')"
check "/sys is read-only" 'echo "$sysrc" | grep -q "rc=0" && false || echo "$sysrc" | grep -qiE "read-only|not permitted"' "$sysrc"

echo "---"
if [ "$fail" -eq 0 ]; then
  echo -e "\033[32mALL ISOLATION CHECKS PASSED\033[0m"
else
  echo -e "\033[31mSOME ISOLATION CHECKS FAILED\033[0m"
fi
exit "$fail"
