# meier

`meier` is the Barbirolli command line client.

On Linux, it calls the local daemon. On macOS, it calls the configured Lima
guest. Lima must already be running. The guest runs the same binary in daemon
mode.

```sh
meier config init
meier daemon setup
meier daemon run
meier vm ps
```

The client prints JSON results to stdout. It prints JSON errors to stderr.
Help, version, SSH, and log output stay raw. The `x` commands stay supported.
