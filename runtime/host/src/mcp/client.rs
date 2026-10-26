//! `palette-host mcp connect`: a stdio MCP client that survives daemon
//! reconnects. It forwards requests once, reports an uncertain in-flight
//! result when a socket closes, and only repeats the protocol handshake.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::util;

const FRAME_LIMIT: usize = 1_048_576;

pub(crate) fn socket_path() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var("PALETTE_SOCKET")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return Ok(util::expand_user(&explicit));
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| login_runtime_dir(&PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))))
        .ok_or("PALETTE_SOCKET is unset and XDG_RUNTIME_DIR is unavailable")?;
    Ok(runtime.join("palette/daemon.sock"))
}

/// MCP clients such as Codex start servers with a filtered environment that
/// drops `XDG_RUNTIME_DIR`. The login manager's per-user directory is used in
/// its place only when it is a real directory this user owns and nobody else
/// can enter.
fn login_runtime_dir(dir: &PathBuf) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir).ok()?;
    let private = meta.is_dir() && meta.uid() == unsafe { libc::getuid() } && meta.mode() & 0o077 == 0;
    private.then(|| dir.clone())
}

/// A connected stream whose server runs as this user. `Err(Err(_))` is a
/// refused or missing socket, which starting the service may fix;
/// `Err(Ok(_))` is a fatal refusal.
fn connect(path: &PathBuf) -> Result<UnixStream, Result<String, std::io::Error>> {
    let stream = UnixStream::connect(path).map_err(Err)?;
    match super::router::peer_uid(&stream) {
        Some(uid) if uid == unsafe { libc::getuid() } => Ok(stream),
        Some(_) => Err(Ok("Palette socket is served by a different user".into())),
        None => Err(Ok(
            "This platform cannot verify the Palette daemon's Unix socket credentials".into(),
        )),
    }
}

pub(crate) fn start_service(path: &PathBuf) -> Result<UnixStream, String> {
    match connect(path) {
        Ok(s) => return Ok(s),
        Err(Ok(fatal)) => return Err(fatal),
        Err(Err(_)) => {}
    }
    let systemctl = util::which("systemctl").ok_or_else(|| {
        format!(
            "Palette daemon is unavailable at {}; systemctl --user is unavailable",
            path.display()
        )
    })?;
    let argv = [
        systemctl.to_string_lossy().into_owned(),
        "--user".into(),
        "start".into(),
        "palette.service".into(),
    ];
    let (code, out, err, timed_out) =
        super::run_captured(&argv, None, Some(Duration::from_secs(30))).map_err(|e| {
            format!(
                "Palette daemon is unavailable and systemd could not start palette.service: {e}"
            )
        })?;
    if timed_out {
        return Err("systemctl --user start palette.service timed out after 30 seconds".into());
    }
    if code != 0 {
        let detail = [err.trim(), out.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .map_or_else(|| format!("exit {code}"), str::to_string);
        return Err(format!(
            "systemctl --user start palette.service failed: {detail}"
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match connect(path) {
            Ok(s) => return Ok(s),
            Err(Ok(fatal)) => return Err(fatal),
            Err(Err(_)) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    Err(format!(
        "palette.service started but Palette did not accept connections at {}",
        path.display()
    ))
}

enum Event {
    Client(String),
    ClientEof,
    ClientError(String),
    Server {
        generation: u64,
        frame: Result<Value, String>,
    },
}

struct PendingRequest {
    generation: u64,
    id: Value,
    method: String,
    initialize_params: Option<Value>,
    internal_initialize: bool,
}

struct Connection {
    writer: UnixStream,
}

impl Connection {
    fn open(path: &PathBuf, generation: u64, events: Sender<Event>) -> Result<Connection, String> {
        let writer = start_service(path)?;
        let reader = writer
            .try_clone()
            .map_err(|e| format!("cloning Palette socket: {e}"))?;
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = match reader
                    .by_ref()
                    .take(FRAME_LIMIT as u64 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(read) => read,
                    Err(error) => {
                        let _ = events.send(Event::Server {
                            generation,
                            frame: Err(format!("reading from Palette daemon: {error}")),
                        });
                        return;
                    }
                };
                if read == 0 {
                    let _ = events.send(Event::Server {
                        generation,
                        frame: Err("Palette daemon closed the connection".into()),
                    });
                    return;
                }
                if line.len() > FRAME_LIMIT {
                    let _ = events.send(Event::Server {
                        generation,
                        frame: Err("Palette daemon frame exceeded 1 MiB".into()),
                    });
                    return;
                }
                match serde_json::from_slice(&line) {
                    Ok(frame) => {
                        if events
                            .send(Event::Server {
                                generation,
                                frame: Ok(frame),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = events.send(Event::Server {
                            generation,
                            frame: Err(format!("invalid Palette daemon frame: {error}")),
                        });
                        return;
                    }
                }
            }
        });
        Ok(Connection { writer })
    }

    fn send(&mut self, line: &str) -> Result<(), String> {
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|e| format!("writing to the Palette daemon: {e}"))
    }

    fn close(&mut self) {
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

struct Proxy {
    path: PathBuf,
    events: Sender<Event>,
    connection: Option<Connection>,
    generation: u64,
    sequence: u64,
    initialized: Option<Value>,
    handshake_ready: bool,
    pending: HashMap<String, PendingRequest>,
    server_requests: HashSet<String>,
}

impl Proxy {
    fn new(path: PathBuf, events: Sender<Event>) -> Result<Proxy, String> {
        let mut proxy = Proxy {
            path,
            events,
            connection: None,
            generation: 0,
            sequence: 0,
            initialized: None,
            handshake_ready: true,
            pending: HashMap::new(),
            server_requests: HashSet::new(),
        };
        proxy.open_connection()?;
        Ok(proxy)
    }

    fn next_id(&mut self) -> Value {
        self.sequence += 1;
        json!(format!(
            "palette-proxy-{}-{}",
            std::process::id(),
            self.sequence
        ))
    }

    fn id_key(id: &Value) -> String {
        id.to_string()
    }

    fn open_connection(&mut self) -> Result<(), String> {
        self.generation += 1;
        self.connection = Some(Connection::open(
            &self.path,
            self.generation,
            self.events.clone(),
        )?);
        self.server_requests.clear();
        if let Some(params) = self.initialized.clone() {
            self.handshake_ready = false;
            let id = self.next_id();
            let key = Self::id_key(&id);
            self.pending.insert(
                key,
                PendingRequest {
                    generation: self.generation,
                    id: id.clone(),
                    method: "initialize".into(),
                    initialize_params: None,
                    internal_initialize: true,
                },
            );
            let frame =
                json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
            if let Err(error) = self.send(&frame) {
                self.pending
                    .retain(|_, request| request.generation != self.generation);
                if let Some(mut connection) = self.connection.take() {
                    connection.close();
                }
                return Err(error);
            }
        } else {
            self.handshake_ready = true;
        }
        Ok(())
    }

    fn send(&mut self, frame: &Value) -> Result<(), String> {
        let bytes = frame.to_string() + "\n";
        self.connection
            .as_mut()
            .ok_or("Palette daemon is disconnected")?
            .send(&bytes)
    }

    fn request_for(
        &mut self,
        line: &str,
        message: &Value,
        out: &mut dyn Write,
    ) -> Result<(), String> {
        let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else {
            return self.send_line(line);
        };
        let key = Self::id_key(&id);
        if self.pending.contains_key(&key) {
            write_frame(
                out,
                &json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32600, "message": "request ID is already in flight"}}),
            )?;
            return Ok(());
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = (method == "initialize")
            .then(|| message.get("params").cloned().unwrap_or_else(|| json!({})));
        self.pending.insert(
            key.clone(),
            PendingRequest {
                generation: self.generation,
                id,
                method,
                initialize_params: params,
                internal_initialize: false,
            },
        );
        if let Err(error) = self.send_line(line) {
            self.disconnect_current(&error, true, out)?;
            return Ok(());
        }
        Ok(())
    }

    fn send_line(&mut self, line: &str) -> Result<(), String> {
        let mut frame = line.to_string();
        if !frame.ends_with('\n') {
            frame.push('\n');
        }
        self.connection
            .as_mut()
            .ok_or("Palette daemon is disconnected")?
            .send(&frame)
    }

    fn client_frame(
        &mut self,
        line: String,
        deferred: &mut VecDeque<String>,
        out: &mut dyn Write,
    ) -> Result<(), String> {
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(error) => {
                write_frame(
                    out,
                    &json!({"jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": format!("Parse error: {error}")}}),
                )?;
                return Ok(());
            }
        };
        let method = message.get("method").and_then(Value::as_str);
        let has_id = message.get("id").is_some_and(|id| !id.is_null());

        // A client response answers a request that originated at the server.
        if method.is_none() && has_id {
            if self.server_requests.remove(&Self::id_key(&message["id"])) {
                if let Err(error) = self.send_line(&line) {
                    self.disconnect_current(&error, true, out)?;
                }
            }
            return Ok(());
        }

        if self.connection.is_none() {
            if method.is_none()
                || !has_id && method.is_some_and(|method| method.starts_with("notifications/"))
            {
                return Ok(());
            }
            if let Err(error) = self.open_connection() {
                if has_id {
                    write_frame(
                        out,
                        &disconnected_error(message["id"].clone(), false, &error),
                    )?;
                }
                fail_deferred(deferred, out, &error)?;
                return Ok(());
            }
        }
        if !self.handshake_ready {
            deferred.push_back(line);
            return Ok(());
        }
        if method.is_some() && has_id {
            self.request_for(&line, &message, out)
        } else {
            if let Err(error) = self.send_line(&line) {
                self.disconnect_current(&error, true, out)?;
            }
            Ok(())
        }
    }

    fn server_frame(
        &mut self,
        generation: u64,
        frame: Value,
        out: &mut dyn Write,
        deferred: &mut VecDeque<String>,
        client_closed: bool,
    ) -> Result<(), String> {
        if generation != self.generation {
            return Ok(());
        }
        if frame.get("method").is_some() {
            if frame.get("id").is_some_and(|id| !id.is_null()) {
                if client_closed {
                    return self.send(&json!({"jsonrpc": "2.0", "id": frame["id"], "error": {
                        "code": -32000, "message": "MCP client input is closed"}}));
                }
                self.server_requests.insert(Self::id_key(&frame["id"]));
            }
            return write_frame(out, &frame);
        }
        let Some(id) = frame.get("id").filter(|id| !id.is_null()) else {
            return Ok(());
        };
        let key = Self::id_key(id);
        let Some(request) = self.pending.remove(&key) else {
            return Ok(());
        };
        if request.generation != generation {
            return Ok(());
        }
        if request.internal_initialize {
            if frame.get("error").is_some() {
                self.disconnect_current("daemon rejected reconnect initialization", false, out)?;
                fail_deferred(deferred, out, "daemon rejected reconnect initialization")?;
                return Ok(());
            }
            self.handshake_ready = true;
            if let Err(error) =
                self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            {
                self.disconnect_current(&error, false, out)?;
                fail_deferred(deferred, out, &error)?;
            }
            return Ok(());
        }
        if request.method == "initialize" && frame.get("error").is_none() {
            self.initialized = request.initialize_params;
        }
        write_frame(out, &frame)
    }

    fn disconnect_current(
        &mut self,
        detail: &str,
        outcome_unknown: bool,
        out: &mut dyn Write,
    ) -> Result<(), String> {
        let generation = self.generation;
        if let Some(mut connection) = self.connection.take() {
            connection.close();
        }
        self.handshake_ready = false;
        self.server_requests.clear();
        let failed: Vec<_> = self
            .pending
            .drain()
            .filter(|(_, request)| request.generation == generation)
            .collect();
        for (_, request) in failed {
            if !request.internal_initialize {
                write_frame(
                    out,
                    &disconnected_error(request.id, outcome_unknown, detail),
                )?;
            }
        }
        Ok(())
    }

    fn reject_server_requests_after_eof(&mut self) -> Result<(), String> {
        let requests = std::mem::take(&mut self.server_requests);
        for key in requests {
            let id = serde_json::from_str::<Value>(&key)
                .map_err(|e| format!("invalid in-flight server request ID: {e}"))?;
            self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32000, "message": "MCP client input is closed"}}))?;
        }
        Ok(())
    }
}

fn fail_deferred(
    deferred: &mut VecDeque<String>,
    out: &mut dyn Write,
    detail: &str,
) -> Result<(), String> {
    while let Some(line) = deferred.pop_front() {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = message.get("id").filter(|id| !id.is_null()) {
            write_frame(out, &disconnected_error(id.clone(), false, detail))?;
        }
    }
    Ok(())
}

fn stdin_reader(events: Sender<Event>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = match reader
                .by_ref()
                .take(FRAME_LIMIT as u64 + 1)
                .read_until(b'\n', &mut line)
            {
                Ok(read) => read,
                Err(error) => {
                    let _ = events.send(Event::ClientError(error.to_string()));
                    return;
                }
            };
            if read == 0 {
                let _ = events.send(Event::ClientEof);
                return;
            }
            if line.len() > FRAME_LIMIT {
                let _ = events.send(Event::ClientError("MCP frame exceeded 1 MiB".into()));
                return;
            }
            if events
                .send(Event::Client(String::from_utf8_lossy(&line).into_owned()))
                .is_err()
            {
                return;
            }
        }
    });
}

fn write_frame(out: &mut dyn Write, frame: &Value) -> Result<(), String> {
    writeln!(out, "{frame}")
        .and_then(|()| out.flush())
        .map_err(|e| format!("writing to MCP client: {e}"))
}

fn disconnected_error(id: Value, outcome_unknown: bool, detail: &str) -> Value {
    let message = if outcome_unknown {
        format!("Palette daemon disconnected while handling this request; its outcome is unknown and it was not replayed. {detail}")
    } else {
        format!("Palette daemon is unavailable; this request was not sent. {detail}")
    };
    json!({"jsonrpc": "2.0", "id": id,
           "error": {"code": -32000, "message": message,
                     "data": {"outcome": if outcome_unknown { "unknown" } else { "not_sent" }, "replayed": false}}})
}

pub(crate) fn main() -> i32 {
    let path = match socket_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Palette MCP client: {error}");
            return 1;
        }
    };
    let (tx, rx) = mpsc::channel();
    stdin_reader(tx.clone());
    let mut proxy = match Proxy::new(path, tx) {
        Ok(proxy) => proxy,
        Err(error) => {
            eprintln!("Palette MCP client: {error}");
            return 1;
        }
    };
    let mut deferred = VecDeque::new();
    let mut out = std::io::stdout().lock();
    let mut client_closed = false;
    loop {
        match rx.recv() {
            Ok(Event::Client(line)) => {
                if let Err(error) = proxy.client_frame(line, &mut deferred, &mut out) {
                    eprintln!("Palette MCP client: {error}");
                    if let Some(mut connection) = proxy.connection.take() {
                        connection.close();
                    }
                    return 1;
                }
            }
            Ok(Event::ClientEof) => {
                client_closed = true;
                if let Err(error) = proxy.reject_server_requests_after_eof() {
                    eprintln!("Palette MCP client: {error}");
                    if let Some(mut connection) = proxy.connection.take() {
                        connection.close();
                    }
                    return 1;
                }
            }
            Ok(Event::ClientError(error)) => {
                eprintln!("Palette MCP client: {error}");
                if let Some(mut connection) = proxy.connection.take() {
                    connection.close();
                }
                return 1;
            }
            Ok(Event::Server {
                generation,
                frame: Ok(frame),
            }) => {
                if let Err(error) =
                    proxy.server_frame(generation, frame, &mut out, &mut deferred, client_closed)
                {
                    eprintln!("Palette MCP client: {error}");
                    if let Some(mut connection) = proxy.connection.take() {
                        connection.close();
                    }
                    return 1;
                }
            }
            Ok(Event::Server {
                generation,
                frame: Err(error),
            }) => {
                if generation == proxy.generation {
                    if let Err(write_error) = proxy
                        .disconnect_current(&error, true, &mut out)
                        .and_then(|()| fail_deferred(&mut deferred, &mut out, &error))
                    {
                        eprintln!("Palette MCP client: {write_error}");
                        return 1;
                    }
                }
            }
            Err(_) => return 0,
        }
        while proxy.connection.is_some() && proxy.handshake_ready {
            let Some(line) = deferred.pop_front() else {
                break;
            };
            if let Err(error) = proxy.client_frame(line, &mut deferred, &mut out) {
                eprintln!("Palette MCP client: {error}");
                if let Some(mut connection) = proxy.connection.take() {
                    connection.close();
                }
                return 1;
            }
        }
        if client_closed
            && deferred.is_empty()
            && !proxy
                .pending
                .values()
                .any(|request| !request.internal_initialize)
        {
            if let Some(mut connection) = proxy.connection.take() {
                connection.close();
            }
            return 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util;
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

    #[test]
    fn login_runtime_dir_must_be_private_and_real() {
        use std::os::unix::fs::PermissionsExt;
        let dir = util::mkdtemp("palette-client-test-").unwrap();
        let runtime = dir.join("runtime");
        assert_eq!(login_runtime_dir(&runtime), None);
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(login_runtime_dir(&runtime), Some(runtime.clone()));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&runtime, &link).unwrap();
        assert_eq!(login_runtime_dir(&link), None);
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(login_runtime_dir(&runtime), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lost_requests_are_reported_uncertain_and_marked_never_replayed() {
        let response = disconnected_error(json!(9), true, "closed");
        assert_eq!(response["id"], 9);
        assert_eq!(
            response["error"]["data"],
            json!({"outcome": "unknown", "replayed": false})
        );
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not replayed"));
    }

    #[test]
    fn closed_client_input_answers_server_requests_instead_of_waiting_forever() {
        let dir = util::mkdtemp("palette-client-closed-input-").unwrap();
        let path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        });
        let (tx, _rx) = mpsc::channel();
        let mut proxy = Proxy::new(path, tx).unwrap();
        let frame =
            json!({"jsonrpc": "2.0", "id": "ask-1", "method": "elicitation/create", "params": {}});
        let mut output = Vec::new();
        proxy
            .server_frame(1, frame, &mut output, &mut VecDeque::new(), true)
            .unwrap();
        let response = server.join().unwrap();
        assert!(
            output.is_empty(),
            "closed client cannot answer this request: {output:?}"
        );
        assert_eq!(response["id"], "ask-1");
        assert_eq!(response["error"]["code"], -32000);
        assert_eq!(response["error"]["message"], "MCP client input is closed");
        if let Some(mut connection) = proxy.connection.take() {
            connection.close();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
