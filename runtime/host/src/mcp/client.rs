//! `palette-host mcp connect`: a disposable stdio MCP client for the
//! persistent daemon. It joins stdin and stdout to the daemon's Unix socket,
//! starting palette.service first when nothing is listening.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::util;

fn socket_path() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var("PALETTE_SOCKET").ok().filter(|v| !v.is_empty()) {
        return Ok(util::expand_user(&explicit));
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok().filter(|v| !v.is_empty())
        .ok_or("PALETTE_SOCKET is unset and XDG_RUNTIME_DIR is unavailable")?;
    Ok(PathBuf::from(runtime).join("palette/daemon.sock"))
}

/// A connected stream whose server runs as this user. `Err(Err(_))` is a
/// refused or missing socket, which starting the service may fix;
/// `Err(Ok(_))` is a fatal refusal.
fn connect(path: &PathBuf) -> Result<UnixStream, Result<String, std::io::Error>> {
    let stream = UnixStream::connect(path).map_err(Err)?;
    match super::router::peer_uid(&stream) {
        Some(uid) if uid == unsafe { libc::getuid() } => Ok(stream),
        Some(_) => Err(Ok("Palette socket is served by a different user".into())),
        None => Err(Ok("This platform cannot verify the Palette daemon's Unix socket credentials".into())),
    }
}

fn start_service(path: &PathBuf) -> Result<UnixStream, String> {
    match connect(path) {
        Ok(s) => return Ok(s),
        Err(Ok(fatal)) => return Err(fatal),
        Err(Err(_)) => {}
    }
    let systemctl = util::which("systemctl")
        .ok_or_else(|| format!("Palette daemon is unavailable at {}; systemctl --user is unavailable", path.display()))?;
    let argv = [systemctl.to_string_lossy().into_owned(), "--user".into(), "start".into(), "palette.service".into()];
    let (code, out, err, timed_out) = super::run_captured(&argv, None, Some(Duration::from_secs(30)))
        .map_err(|e| format!("Palette daemon is unavailable and systemd could not start palette.service: {e}"))?;
    if timed_out {
        return Err("systemctl --user start palette.service timed out after 30 seconds".into());
    }
    if code != 0 {
        let detail = [err.trim(), out.trim()].into_iter().find(|s| !s.is_empty()).map_or_else(|| format!("exit {code}"), str::to_string);
        return Err(format!("Palette daemon is unavailable and systemd could not start palette.service: {detail}"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match connect(path) {
            Ok(s) => return Ok(s),
            Err(Ok(fatal)) => return Err(fatal),
            Err(Err(_)) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    Err(format!("palette.service started but Palette did not accept connections at {}", path.display()))
}

/// Copies stdin to the socket until EOF, then half-closes it, while the
/// daemon's replies stream to stdout until it closes its side.
fn relay(stream: UnixStream) -> std::io::Result<()> {
    let mut reader = stream.try_clone()?;
    let output = std::thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        let mut buf = [0u8; 65536];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if stdout.write_all(&buf[..n]).and_then(|()| stdout.flush()).is_err() {
                return;
            }
        }
    });
    let mut writer = &stream;
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 65536];
    while let Ok(n @ 1..) = stdin.read(&mut buf) {
        if writer.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Write);
    let _ = output.join();
    Ok(())
}

pub(crate) fn main() -> i32 {
    match socket_path().and_then(|p| start_service(&p)) {
        Ok(stream) => {
            let _ = relay(stream);
            0
        }
        Err(e) => {
            eprintln!("Palette MCP client: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn connects_to_a_socket_this_user_serves_and_retries_a_missing_one() {
        let dir = util::mkdtemp("palette-client-test-").unwrap();
        let path = dir.join("daemon.sock");
        assert!(matches!(connect(&path), Err(Err(_))));
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(connect(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
