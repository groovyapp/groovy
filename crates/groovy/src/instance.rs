//! One running Groovy per data directory. A second launch hands its arguments to the running
//! copy over a socket in the data directory, and exits. The running copy takes them on its next
//! tick: a link to act on, or a request to bring the window forward.
//!
//! Only Unix has the socket here. Elsewhere every launch is the first one.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::deeplink;

/// What a launch asked the running app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Bring the window forward, or open it. Only another launch asks for it, which needs Unix.
    #[cfg_attr(not(unix), allow(dead_code))]
    Show,
    /// A `groovy://` link, as it was given.
    Link(String),
}

/// What this launch is, after checking for a running copy.
pub enum Claim {
    /// This is the running copy. Its arguments have been queued like any other request.
    First,
    /// A running copy has the arguments now, so this launch should end. Only on Unix.
    #[cfg_attr(not(unix), allow(dead_code))]
    Forwarded,
}

/// Requests received since the last call, oldest first.
static INCOMING: Mutex<Vec<Incoming>> = Mutex::new(Vec::new());

pub fn take_incoming() -> Vec<Incoming> {
    std::mem::take(&mut *INCOMING.lock().unwrap_or_else(PoisonError::into_inner))
}

fn push(item: Incoming) {
    INCOMING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(item);
}

/// The request one launch sends to the running copy, one line each: `show`, or `link <text>`.
#[cfg(unix)]
fn messages(args: &[String]) -> Vec<String> {
    let mut lines = vec!["show".to_owned()];
    lines.extend(
        args.iter()
            .filter(|arg| arg.starts_with(deeplink::SCHEME))
            .map(|arg| format!("link {arg}")),
    );
    lines
}

/// A first launch's links are acted on like those from a later launch. It does not open the
/// window on its own, so `--background` still starts with no window.
fn queue_own_links(args: &[String]) {
    for arg in args.iter().filter(|arg| arg.starts_with(deeplink::SCHEME)) {
        push(Incoming::Link(arg.clone()));
    }
}

/// Parses one line from another launch.
#[cfg(unix)]
fn parse_line(line: &str) -> Option<Incoming> {
    let line = line.trim_end();
    if line == "show" {
        return Some(Incoming::Show);
    }
    line.strip_prefix("link ")
        .map(|text| Incoming::Link(text.to_owned()))
}

/// Checks for a running copy on `socket`. If there is one, sends it this launch's arguments.
/// Otherwise becomes the running copy and listens for later launches.
#[cfg(unix)]
pub fn claim(socket: &Path, args: &[String]) -> std::io::Result<Claim> {
    use std::io::Write;
    use std::os::unix::net::{UnixListener, UnixStream};

    if let Ok(mut stream) = UnixStream::connect(socket) {
        for line in messages(args) {
            writeln!(stream, "{line}")?;
        }
        return Ok(Claim::Forwarded);
    }
    // Nothing answered, so whatever file is there is left from a launch that ended.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    std::thread::Builder::new()
        .name("groovy-instance".to_owned())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(stream)
                        .lines()
                        .map_while(Result::ok)
                    {
                        if let Some(item) = parse_line(&line) {
                            push(item);
                        }
                    }
                });
            }
        })?;
    queue_own_links(args);
    Ok(Claim::First)
}

/// Off Unix there is no socket, so every launch is the first.
#[cfg(not(unix))]
pub fn claim(_socket: &Path, args: &[String]) -> std::io::Result<Claim> {
    queue_own_links(args);
    Ok(Claim::First)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_launch_sends_show_and_any_links_it_was_given() {
        let args = vec![
            "groovy".to_owned(),
            "--background".to_owned(),
            "groovy://groovygroup1abc".to_owned(),
        ];
        assert_eq!(messages(&args), ["show", "link groovy://groovygroup1abc"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_second_launch_hands_its_link_to_the_first_and_ends() {
        let socket = std::env::temp_dir().join(format!("groovy-test-{}.sock", std::process::id()));
        let first = vec!["groovy".to_owned(), "groovy://first".to_owned()];
        assert!(matches!(claim(&socket, &first).unwrap(), Claim::First));
        let second = vec![
            "groovy".to_owned(),
            "groovy://groovygroup1second".to_owned(),
        ];
        assert!(matches!(claim(&socket, &second).unwrap(), Claim::Forwarded));

        // The first launch reads the second one's request on its own thread, so wait for it.
        let mut seen = Vec::new();
        for _ in 0..100 {
            seen.extend(take_incoming());
            if seen
                .iter()
                .any(|item| *item == Incoming::Link("groovy://groovygroup1second".to_owned()))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.contains(&Incoming::Link("groovy://first".to_owned())));
        assert!(seen.contains(&Incoming::Show));
        assert!(seen.contains(&Incoming::Link("groovy://groovygroup1second".to_owned())));
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn a_line_from_another_launch_is_read_back() {
        assert_eq!(parse_line("show\n"), Some(Incoming::Show));
        assert_eq!(
            parse_line("link groovy://abc"),
            Some(Incoming::Link("groovy://abc".to_owned()))
        );
        assert_eq!(parse_line("something else"), None);
    }
}
