//! Argument validation extracted from the current production configuration.

use clap::{
    Parser,
    error::ErrorKind,
};

include!(concat!(env!("OUT_DIR"), "/argument_methods.rs"));

#[test]
fn valid_members_work_in_launch_and_signal_modes() {
    let longest = format!("A{}", "9".repeat(254));
    for member in ["xpop", "Terminal_2", "_", "_9", &longest] {
        let host = cfg::Args::try_parse_from([
            "xpop",
            "--dbus-member",
            member,
            "terminal",
        ])
        .unwrap();
        let signal = cfg::Args::try_parse_from([
            "xpop",
            "--signal",
            "--dbus-member",
            member,
        ])
        .unwrap();
        assert_eq!(host.dbus_member, member);
        assert_eq!(signal.dbus_member, member);
    }
}

#[test]
fn invalid_members_are_rejected_before_launch_or_signal() {
    let oversized = "A".repeat(256);
    for member in [
        "",
        "9terminal",
        "terminal-2",
        "terminal.name",
        "terminal/name",
        "terminal name",
        "terminal\n",
        "términal",
        &oversized,
    ] {
        for mode in ["terminal", "--signal"] {
            let error = cfg::Args::try_parse_from([
                "xpop",
                "--dbus-member",
                member,
                mode,
            ])
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{member:?}");
        }
    }
}
