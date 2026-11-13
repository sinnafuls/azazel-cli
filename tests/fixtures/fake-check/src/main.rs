//! A stand-in for a linter and for `gitleaks`, for tests. It does what its arguments say, in the
//! order given, and ignores every argument it does not know (the file a checker is handed, a
//! real program's own flags):
//!
//! - `--line <text>` prints `<text>` on standard output; `--err <text>` on standard error.
//! - `--from <file>` prints the content of `<file>`, so a test can change what is found between
//!   two runs, and makes the exit code 1 when there was something to print, as a linter does.
//! - `--log <file>` appends one line to `<file>`, so a test can count the runs.
//! - `--sleep-ms <n>` waits; `--exit <code>` sets the exit code (0 otherwise).
//!
//! The variable `FAKE_CHECK_ARGS` holds more of the same, one argument per line, for a copy of
//! this program that stands in for a real one and so cannot be given arguments. In them `{file}`
//! stands for the last argument of the command line.
//!
//! Run as `gitleaks` is (`detect ... --report-path <out> --source <dir>`), it copies
//! `<dir>/.fake-leaks.json` to `<out>` and exits with 1, or with 0 when that file is missing.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

fn value(args: &[String], name: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn leaks(args: &[String]) -> ExitCode {
    let (Some(out), Some(source)) = (value(args, "--report-path"), value(args, "--source")) else {
        eprintln!("fake-check: detect needs --report-path and --source");
        return ExitCode::from(2);
    };
    match std::fs::read(Path::new(&source).join(".fake-leaks.json")) {
        Ok(report) => {
            if std::fs::write(&out, report).is_err() {
                return ExitCode::from(2);
            }
            ExitCode::from(1)
        }
        Err(_) => ExitCode::SUCCESS,
    }
}

fn main() -> ExitCode {
    let given: Vec<String> = std::env::args().skip(1).collect();
    if given.first().is_some_and(|first| first == "detect") {
        return leaks(&given);
    }
    let file = given.last().cloned().unwrap_or_default();
    let mut args = given;
    if let Ok(more) = std::env::var("FAKE_CHECK_ARGS") {
        args.extend(more.lines().map(|line| line.replace("{file}", &file)));
    }
    let mut code = 0;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut next = || args.next().unwrap_or_default();
        match arg.as_str() {
            "--line" => println!("{}", next()),
            "--err" => eprintln!("{}", next()),
            "--from" => {
                if let Ok(text) = std::fs::read_to_string(next())
                    && !text.trim().is_empty()
                {
                    print!("{text}");
                    code = 1;
                }
            }
            "--log" => {
                let log = std::fs::OpenOptions::new().create(true).append(true).open(next());
                if let Ok(mut log) = log {
                    let _ = writeln!(log, "run");
                }
            }
            "--sleep-ms" => {
                let _ = std::io::stdout().flush();
                std::thread::sleep(std::time::Duration::from_millis(next().parse().unwrap_or(0)));
            }
            "--exit" => code = next().parse().unwrap_or(1),
            _ => {}
        }
    }
    ExitCode::from(code)
}
