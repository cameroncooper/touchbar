#!/usr/bin/env bash
# Run a command inside the kind of session this project's tests assume.
#
# The supervisor's confinement is not optional: it requires its own cgroup to
# be a private user-owned directory, and several capability backends need a
# session bus. A graphical login provides both. A hosted CI runner provides
# neither — its job cgroup is owned by root and no session bus exists — so the
# sandbox, cgroup, and portal tests fail there for environmental reasons rather
# than because anything regressed.
#
# This wrapper reproduces those two properties: a transient systemd scope with
# a delegated, user-owned cgroup, and a private session bus. On a machine that
# already has both, it runs the command directly.
set -euo pipefail

if [[ $# -eq 0 ]]; then
    echo "usage: ci-session.sh COMMAND [ARGUMENTS...]" >&2
    exit 2
fi

have_session_bus() {
    [[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]]
}

# This must match the supervisor's own check exactly. Testing only for
# ownership is not enough: a cgroup owned by this user but writable by group or
# other is still rejected, and treating it as usable makes every caller skip
# the delegated scope it actually needs.
own_cgroup() {
    local relative root directory owner mode
    relative=$(awk -F: '$1 == "0" { print $3 }' /proc/self/cgroup 2>/dev/null) || return 1
    [[ -n "$relative" ]] || return 1
    root=/sys/fs/cgroup
    directory="$root${relative}"
    [[ -d "$directory" ]] || return 1
    owner=$(stat -c %u "$directory" 2>/dev/null) || return 1
    mode=$(stat -c %a "$directory" 2>/dev/null) || return 1
    [[ "$owner" == "$(id -u)" ]] || return 1
    (( 8#$mode & 8#022 )) && return 1
    return 0
}

command=("$@")
if ! have_session_bus; then
    command=(dbus-run-session -- "${command[@]}")
fi

if own_cgroup; then
    exec "${command[@]}"
fi

# Delegate=yes makes systemd chown the scope's cgroup to the given user, which
# is exactly the ownership the supervisor checks for.
exec sudo --preserve-env=PATH,HOME,CARGO_HOME,RUSTUP_HOME,CARGO_TERM_COLOR,RUSTFLAGS \
    systemd-run --quiet --scope --same-dir --collect \
    --uid="$(id -u)" --gid="$(id -g)" --property=Delegate=yes \
    -- "${command[@]}"
