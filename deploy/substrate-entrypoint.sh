#!/bin/sh
# Repair only the actor's private writable directories after image extraction.
set -eu

if [ "${1-}" != serve ]; then
    exec /usr/local/libexec/branchyard-bridge "$@"
fi

fail() {
    printf '%s\n' "branchyard-substrate: $*" >&2
    exit 1
}

actor_uid=${BRANCHYARD_ACTOR_UID:-1000}
actor_gid=${BRANCHYARD_ACTOR_GID:-1000}
actor_workdir=${BRANCHYARD_ACTOR_WORKDIR:-/workspace}
actor_home=${BRANCHYARD_ACTOR_HOME:-/branchyard/home}

for value in "$actor_uid" "$actor_gid"; do
    case "$value" in ''|*[!0-9]*|0*) fail 'actor UID and GID must be positive decimal integers without leading zeros' ;; esac
    [ "${#value}" -le 10 ] && [ "$value" -lt 4294967295 ] || fail 'actor UID or GID is out of range'
done

run_as=
next_run_as=0
seen_run_as=0
for argument do
    if [ "$next_run_as" = 1 ]; then
        run_as=$argument
        next_run_as=0
    else
        case "$argument" in
            --run-as)
                [ "$seen_run_as" = 0 ] || fail 'duplicate --run-as'
                seen_run_as=1
                next_run_as=1
                ;;
            --run-as=*)
                [ "$seen_run_as" = 0 ] || fail 'duplicate --run-as'
                seen_run_as=1
                run_as=${argument#--run-as=}
                ;;
        esac
    fi
done
[ "$next_run_as" = 0 ] && [ "$run_as" = "$actor_uid:$actor_gid" ] ||
    fail '--run-as must match BRANCHYARD_ACTOR_UID:BRANCHYARD_ACTOR_GID'
[ "$(id -u)" = 0 ] || fail 'the bridge bootstrap must start as root'

for directory in "$actor_workdir" "$actor_home"; do
    case "$directory" in
        /) fail 'workspace and HOME cannot own the actor root' ;;
        /*) ;;
        *) fail 'workspace and HOME must be absolute directory paths' ;;
    esac
    for reserved in /run/branchyard /var/lib/branchyard-bridge; do
        case "$directory/" in "$reserved/"*) fail 'workspace and HOME overlap bridge identity or state' ;; esac
        case "$reserved/" in "$directory/"*) fail 'workspace and HOME contain bridge identity or state' ;; esac
    done
    # Reject symlinks and noncanonical paths before privileged directory changes.
    [ "$(realpath -m -- "$directory")" = "$directory" ] ||
        fail 'workspace and HOME must be canonical paths without symlink components'
done
case "$actor_workdir/" in "$actor_home/"*) fail 'workspace and HOME must be separate directories' ;; esac
case "$actor_home/" in "$actor_workdir/"*) fail 'workspace and HOME must be separate directories' ;; esac

chmod 0755 /
mkdir -p -- "$actor_workdir" "$actor_home"
# chown first to root so chmod needs ownership, not CAP_FOWNER; hand over last.
chown 0:0 -- "$actor_workdir" "$actor_home"
chmod 0700 -- "$actor_workdir" "$actor_home"
chown "$actor_uid:$actor_gid" -- "$actor_workdir" "$actor_home"
exec /usr/local/libexec/branchyard-bridge "$@"
