use clap::Parser;
use meier::cli::{Cli, Command, ConfigCommand, DaemonCommand, OciCommand};

#[test]
fn parses_the_complete_nested_command_surface() {
    let cases = [
        (vec!["meier", "config", "init"], "config"),
        (vec!["meier", "daemon", "setup"], "daemon"),
        (vec!["meier", "daemon", "run"], "daemon"),
        (vec!["meier", "vm", "create", "--vcpu-count", "2"], "vm"),
        (vec!["meier", "vm", "ps"], "vm"),
        (vec!["meier", "vm", "show", "0"], "vm"),
        (vec!["meier", "vm", "status", "0"], "vm"),
        (vec!["meier", "vm", "start", "0"], "vm"),
        (vec!["meier", "vm", "shutdown", "0"], "vm"),
        (vec!["meier", "vm", "delete", "0"], "vm"),
        (vec!["meier", "vm", "ssh", "0", "--", "uname", "-a"], "vm"),
        (vec!["meier", "vm", "logs", "0", "--pull"], "vm"),
        (vec!["meier", "vm", "logs", "0", "-p"], "vm"),
        (vec!["meier", "oci", "pull", "alpine:latest"], "oci"),
        (vec!["meier", "oci", "run", "alpine:latest"], "oci"),
        (vec!["meier", "oci", "stop", "alpine:latest"], "oci"),
        (vec!["meier", "oci", "rm", "alpine:latest"], "oci"),
    ];

    for (argv, expected) in cases {
        let cli = Cli::try_parse_from(argv).expect("command should parse");
        match (expected, cli.command) {
            ("config", Command::Config(ConfigCommand::Init))
            | ("daemon", Command::Daemon(DaemonCommand::Setup | DaemonCommand::Run))
            | ("vm", Command::Vm(_))
            | (
                "oci",
                Command::Oci(
                    OciCommand::Pull(_)
                    | OciCommand::Run(_)
                    | OciCommand::Stop(_)
                    | OciCommand::Rm(_),
                ),
            ) => {}
            _ => panic!("parsed command was routed to the wrong namespace"),
        }
    }
}

#[test]
fn rejects_invalid_vm_ids_and_conflicting_log_modes() {
    assert!(Cli::try_parse_from(["meier", "vm", "status", "16384"]).is_err());
    assert!(Cli::try_parse_from(["meier", "vm", "logs", "0", "--attach", "--pull"]).is_err());
}

#[cfg(feature = "daemon")]
#[test]
fn parses_the_hidden_daemon_mode_in_the_same_binary() {
    let cli = Cli::try_parse_from([
        "meier",
        "--config",
        "/etc/meier/config.json",
        "__daemon",
        "--runtime-dir",
        "/var/lib/meier",
    ])
    .expect("daemon mode should parse when the feature is enabled");
    let Command::InternalDaemon(args) = cli.command else {
        panic!("expected the hidden daemon command");
    };
    assert_eq!(
        args.runtime_dir.as_deref(),
        Some(std::path::Path::new("/var/lib/meier"))
    );
}
