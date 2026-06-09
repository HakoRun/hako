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
"$HAKO" init >/dev/null

# HAKO_RUN_FLAGS lets callers pass extra `run` flags (e.g. --no-workspace while
# writable-rootfs/volume support is still landing).
run() { "$HAKO" run ${HAKO_RUN_FLAGS:-} "$BRANCH" sh -c "$1" 2>/dev/null; }

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

echo "---"
if [ "$fail" -eq 0 ]; then
  echo -e "\033[32mALL ISOLATION CHECKS PASSED\033[0m"
else
  echo -e "\033[31mSOME ISOLATION CHECKS FAILED\033[0m"
fi
exit "$fail"
