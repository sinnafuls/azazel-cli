//! A stand-in for `npm` for tests. `fake-npm install --prefix <dir> [flags] <package>…` makes the
//! program of each package, as npm does, in `<dir>/node_modules/.bin`: a copy of `fake-lsp`, which
//! must sit beside this program, named `pyright-langserver` for `pyright` and after the package
//! for any other. Every call adds a line to `npm.log` beside this program. `fake-npm fail …`
//! as the first argument is not special; a `--fail` flag makes the call print an error and exit 1.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let me = std::env::current_exe().unwrap_or_default();
    let beside = me.parent().map(PathBuf::from).unwrap_or_default();
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(beside.join("npm.log"))
    {
        let _ = writeln!(log, "{}", args.join("\t"));
    }
    if args.iter().any(|arg| arg == "--fail") {
        eprintln!("npm ERR! network unreachable");
        return ExitCode::from(1);
    }
    let Some(prefix) = args
        .iter()
        .position(|arg| arg == "--prefix")
        .and_then(|at| args.get(at + 1))
    else {
        eprintln!("fake-npm: only `install --prefix <dir> <package>…` is supported");
        return ExitCode::from(2);
    };
    let server = beside.join(format!("fake-lsp{}", std::env::consts::EXE_SUFFIX));
    let bin = PathBuf::from(prefix).join("node_modules").join(".bin");
    if let Err(e) = std::fs::create_dir_all(&bin) {
        eprintln!("fake-npm: {e}");
        return ExitCode::from(1);
    }
    let mut skip = false;
    for arg in args.iter().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--prefix" {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        let name = if arg == "pyright" {
            "pyright-langserver"
        } else {
            arg.as_str()
        };
        let target = bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if let Err(e) = std::fs::copy(&server, &target) {
            eprintln!("fake-npm: could not copy {}: {e}", server.display());
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}
