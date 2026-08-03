# meier

`meier` is the cross-platform command-line client for the Barbirolli
Firecracker VM service. It speaks to a local daemon on Linux and uses a
configured, already-running Lima guest on macOS. The daemon is the same
executable built with the `daemon` feature and launched through its hidden
`__daemon` command; the guest never invokes the public client command.

```sh
meier config init
meier daemon setup
meier daemon run
```

In another terminal, use `meier vm` and `meier oci` commands. JSON responses are
written to stdout and tracing diagnostics are written to stderr. Errors are
JSON on stderr, while help/version, SSH sessions, and log streams remain raw.
The existing `x` and Bash commands remain supported.
