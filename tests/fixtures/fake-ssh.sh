#!/bin/sh
# A fake `ssh` binary used by the end-to-end tests.
#
# Behaviour is driven by the SSHPF_FAKE_* environment variables so a single
# script can impersonate every interesting success/failure mode of a real ssh
# without touching the network.
#
#   SSHPF_FAKE_MODE = ok | port-in-use | permission-denied | auth-failed |
#                     dns-failure | refused | timeout | host-key |
#                     exit-immediately | up-then-die | target-refused
#
# Everything is written to stderr, like real ssh's debug output.

MODE="${SSHPF_FAKE_MODE:-ok}"

# `ssh -G <host>` is used to resolve connection parameters. Answer it with a
# minimal but well-formed dump so the synthetic-config path stays fast.
want_target=0
for arg in "$@"; do
    if [ "$want_target" = "1" ]; then
        if [ "$arg" = "-G" ]; then :; fi
    fi
    [ "$arg" = "-G" ] && want_target=1
done
case " $* " in
    *" -G "*)
        echo "host $*"
        echo "hostname 127.0.0.1"
        echo "user wangcan"
        echo "port 22222"
        echo "proxyjump none"
        echo "identityfile ~/.ssh/id_rsa"
        echo "stricthostkeychecking no"
        echo "userknownhostsfile /dev/null"
        echo "batchmode yes"
        echo "loglevel INFO"
        exit 0
        ;;
esac

# Extract the spec that follows -L / -R / -D, and the intended local port
# (field 2 of "addr:port:host:port", field 2 of "addr:port").
SPEC=""
PORT=""
want_spec=0
for arg in "$@"; do
    if [ "$want_spec" = "1" ]; then
        SPEC="$arg"
        PORT=$(printf '%s' "$arg" | cut -d: -f2)
        want_spec=0
        continue
    fi
    case "$arg" in
        -L|-R|-D) want_spec=1 ;;
    esac
done

# Record the argument vector so tests can assert on the exact command line.
if [ -n "$SSHPF_FAKE_ARGLOG" ]; then
    printf 'ARGS %s\n' "$*" >> "$SSHPF_FAKE_ARGLOG"
fi

ready_local() {
    echo "debug1: Local connections to 127.0.0.1:$PORT forwarded to remote address 127.0.0.1:22" >&2
    echo "debug1: Local forwarding listening on 127.0.0.1 port $PORT." >&2
}

case "$MODE" in
    port-in-use)
        ready_local
        echo "bind [127.0.0.1]:$PORT: Address already in use" >&2
        echo "channel_setup_fwd_listener_tcpip: cannot listen to port: $PORT" >&2
        echo "Could not request local forwarding." >&2
        exit 255
        ;;
    permission-denied)
        echo "bind [127.0.0.1]:$PORT: Permission denied" >&2
        echo "channel_setup_fwd_listener_tcpip: cannot listen to port: $PORT" >&2
        echo "Could not request local forwarding." >&2
        exit 255
        ;;
    auth-failed)
        echo "wangcan@127.0.0.1: Permission denied (publickey)." >&2
        exit 255
        ;;
    dns-failure)
        echo "ssh: Could not resolve hostname no-such-host.invalid: Temporary failure in name resolution" >&2
        exit 255
        ;;
    refused)
        echo "ssh: connect to host 127.0.0.1 port 1: Connection refused" >&2
        exit 255
        ;;
    timeout)
        echo "ssh: connect to host 10.255.255.1 port 22: Connection timed out" >&2
        exit 255
        ;;
    host-key)
        echo "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@" >&2
        echo "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @" >&2
        echo "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@" >&2
        echo "Host key verification failed." >&2
        exit 255
        ;;
    exit-immediately)
        exit 255
        ;;
    up-then-die)
        ready_local
        echo "debug1: Entering interactive session." >&2
        sleep 0.2
        echo "Connection to 127.0.0.1 closed by remote host." >&2
        exit 255
        ;;
    target-refused)
        ready_local
        echo "debug1: Entering interactive session." >&2
        sleep 0.2
        echo "debug1: channel 2: open failed: connect failed: Connection refused" >&2
        sleep 30
        ;;
    ok|*)
        ready_local
        echo "debug1: Authenticated to 127.0.0.1 ([127.0.0.1]:22)." >&2
        echo "debug1: Entering interactive session." >&2
        # Stay alive like `ssh -N` would.
        exec sleep 30
        ;;
esac
