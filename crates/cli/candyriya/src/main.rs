//! The `candyriya` binary — a thin shell around the library's `run`.
//!
//! `--version` and `--help` short-circuit here; everything else is the
//! composition root in [`candyriya::run`].

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = candyriya::Args::parse();
    if args.version {
        println!("{}", candyriya::VERSION_LINE);
        println!("{}", candyriya::BANNER);
        return ExitCode::SUCCESS;
    }
    candyriya::run_to_exit_code(args)
}
