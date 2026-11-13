//! Stands in for `ssh` in tests: `fake-ssh [options] <host> <command>` runs the command on this
//! machine, with this process's input and output, and exits with its status.
//!
//! The command is split into words the way a POSIX shell would split what azazel sends: words
//! in single quotes are taken as they are, a backslash keeps the character after it. A leading
//! `env NAME=value ...` sets variables for the command, as the program of that name does.

use std::process::{Command, ExitCode};

/// The words of `line`.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                for quoted in chars.by_ref() {
                    if quoted == '\'' {
                        break;
                    }
                    word.push(quoted);
                }
            }
            '\\' => {
                started = true;
                word.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if std::mem::take(&mut started) {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    // Options first: `-T`, and `-o Name=value`.
    let host = loop {
        match args.next().as_deref() {
            Some("-o") => drop(args.next()),
            Some(option) if option.starts_with('-') => {}
            Some(host) => break host.to_string(),
            None => {
                eprintln!("fake-ssh: no host");
                return ExitCode::from(255);
            }
        }
    };
    // What a machine that cannot be reached looks like.
    if host == "unreachable" {
        eprintln!("ssh: connect to host unreachable port 22: Connection refused");
        return ExitCode::from(255);
    }
    let line: Vec<String> = args.collect();
    let mut words = words(&line.join(" ")).into_iter().peekable();
    let mut variables = Vec::new();
    if words.peek().map(String::as_str) == Some("env") {
        words.next();
        while let Some((name, value)) = words.peek().and_then(|word| word.split_once('=')) {
            variables.push((name.to_string(), value.to_string()));
            words.next();
        }
    }
    let Some(program) = words.next() else {
        eprintln!("fake-ssh: no command");
        return ExitCode::from(255);
    };
    match Command::new(&program).args(words).envs(variables).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(255) as u8),
        Err(e) => {
            // What a shell says when the program is not there.
            eprintln!("sh: {program}: command not found ({e})");
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_split_as_a_shell_splits_it() {
        assert_eq!(
            words(r"azazel --cwd '/srv/my app' rpc 'it'\''s' ''"),
            ["azazel", "--cwd", "/srv/my app", "rpc", "it's", ""]
        );
    }
}
