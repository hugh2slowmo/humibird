//! A client for context servers that speak the Model Context Protocol (MCP).
//!
//! One server is one child process and one connection. The protocol is
//! newline-delimited JSON-RPC 2.0 over stdin and stdout, and it is synchronous on
//! purpose: the turn runs tool calls away from the UI thread, so a call that waits
//! for a process is no different from a call that waits for `grep`.
//!
//! The parts a caller needs are [`Server::connect`], [`Server::tools`] and
//! [`Server::call`]. Everything else — ids, framing, timeouts, what to do with a
//! notification that arrives mid-request — stays in here.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::{AsyncReadExt as _, executor::block_on};
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde_json::{Value, json};

/// What this client understands. A server that speaks an older version answers
/// with its own and we take it: the parts used here are in all of them.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long any one request may take unless the server's configuration says
/// otherwise. Long enough for a slow tool, short enough that a hung server does
/// not hold the turn forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A tool the server offers. `input_schema` is passed to the model unread, apart
/// from being made an object when the server sent nothing usable.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// How to start a server.
#[derive(Clone, Debug)]
pub struct Options {
    endpoint: Endpoint,
    timeout: Option<Duration>,
}

/// Where the server is: a command to run, or a URL to talk to.
#[derive(Clone, Debug)]
enum Endpoint {
    Stdio {
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl Options {
    pub fn stdio(program: impl Into<String>) -> Self {
        Options {
            endpoint: Endpoint::Stdio {
                program: program.into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            timeout: None,
        }
    }

    pub fn http(url: impl Into<String>) -> Self {
        Options {
            endpoint: Endpoint::Http {
                url: url.into(),
                headers: Vec::new(),
            },
            timeout: None,
        }
    }

    pub fn args(mut self, args: Vec<String>) -> Self {
        if let Endpoint::Stdio { args: to, .. } = &mut self.endpoint {
            *to = args;
        }
        self
    }

    pub fn env(mut self, env: Vec<(String, String)>) -> Self {
        if let Endpoint::Stdio { env: to, .. } = &mut self.endpoint {
            *to = env;
        }
        self
    }

    pub fn headers(mut self, headers: Vec<(String, String)>) -> Self {
        if let Endpoint::Http { headers: to, .. } = &mut self.endpoint {
            *to = headers;
        }
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// A started server: a child process, an initialised connection, and the place
/// its answers arrive.
pub struct Server {
    name: String,
    transport: Box<dyn Transport>,
    next_id: i64,
    timeout: Duration,
    /// The protocol version the server answered with, for the log.
    version: Option<String>,
    /// Whether the connection can still be trusted. A timed-out request leaves a
    /// reply that could be mistaken for the next one's, so the server is retired
    /// instead of being reused.
    stale: Option<String>,
}

impl Server {
    /// Starts the server and performs the handshake.
    pub fn connect(name: &str, options: &Options) -> Result<Self> {
        let timeout = options.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let transport: Box<dyn Transport> = match &options.endpoint {
            Endpoint::Stdio { program, args, env } => Box::new(StdioTransport::spawn(
                name,
                program,
                args.clone(),
                env.clone(),
            )?),
            Endpoint::Http { url, headers } => Box::new(HttpTransport::new(
                name.to_owned(),
                url.clone(),
                headers.clone(),
            )),
        };
        let mut server = Self::with_transport(name, transport, timeout);
        server.handshake()?;
        Ok(server)
    }

    pub(crate) fn with_transport(
        name: &str,
        transport: Box<dyn Transport>,
        timeout: Duration,
    ) -> Self {
        Server {
            name: name.to_owned(),
            transport,
            next_id: 1,
            timeout,
            version: None,
            stale: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The protocol version the server agreed to.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn handshake(&mut self) -> Result<()> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "humibird",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        self.version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_owned);
        log::info!(
            "mcp {} speaks {:?}",
            self.name,
            self.version.as_deref().unwrap_or("an unnamed version")
        );
        // The handshake is only complete once the server is told it can send
        // notifications; before that its `initialized` would be an error on some.
        self.notify("notifications/initialized", json!({}))
    }

    /// Every tool the server offers, following its pagination to the end.
    pub fn tools(&mut self) -> Result<Vec<RemoteTool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        // A server that keeps handing out a cursor must not keep the turn here.
        for _ in 0..100 {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params)?;
            let Some(list) = result.get("tools").and_then(Value::as_array) else {
                bail!("mcp {} answered tools/list with no list", self.name);
            };
            for tool in list {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    log::warn!("mcp {} offered a tool with no name", self.name);
                    continue;
                };
                tools.push(RemoteTool {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: input_schema(tool.get("inputSchema")),
                });
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_owned()),
                None => return Ok(tools),
            }
        }
        log::warn!("mcp {} kept paginating tools/list; stopping", self.name);
        Ok(tools)
    }

    /// Runs one of the server's tools and returns what it says. A tool that
    /// reports failure is an error, so the model is told the call did not work.
    pub fn call(&mut self, tool: &str, arguments: &Value) -> Result<String> {
        let result = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )?;
        let failed = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or_default();
        let text = content_text(&result);
        if failed {
            bail!(
                "{}",
                if text.is_empty() {
                    "the call failed"
                } else {
                    &text
                }
            );
        }
        Ok(text)
    }

    /// The last error the connection suffered, if it can no longer be used.
    pub fn stale(&self) -> Option<&str> {
        self.stale.as_deref()
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&message)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        if let Some(stale) = &self.stale {
            bail!("mcp {} stopped answering: {stale}", self.name);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;

        loop {
            let message = self.receive(method)?;
            let value: Value = match serde_json::from_str(&message) {
                Ok(value) => value,
                Err(error) => bail!("mcp {} sent something unreadable: {error}", self.name),
            };
            if value.get("id").and_then(Value::as_i64) != Some(id) {
                // Notifications are expected; a reply to someone else's request is
                // not, since calls are made one at a time.
                if value.get("id").is_none() {
                    log::debug!("mcp {} said {}", self.name, message);
                } else {
                    log::warn!(
                        "mcp {} answered a request nobody made: {message}",
                        self.name
                    );
                }
                continue;
            }
            if let Some(error) = value.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the server refused the call");
                bail!("mcp {}: {text}", self.name);
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        self.transport
            .send(&message.to_string())
            .with_context(|| format!("failed to write to mcp {}", self.name))
    }

    fn receive(&mut self, method: &str) -> Result<String> {
        match self.transport.receive(self.timeout) {
            Ok(Some(message)) => Ok(message),
            Ok(None) => {
                let reason = format!("it closed its output during {method}");
                self.stale = Some(reason.clone());
                bail!("mcp {} stopped: {reason}", self.name)
            }
            Err(error) => {
                let reason = error.to_string();
                self.stale = Some(reason.clone());
                bail!("mcp {} stopped: {reason}", self.name)
            }
        }
    }
}

/// The text of a tool result. Content that is not text — an image, a resource —
/// is named rather than dropped, so the model knows something came back.
fn content_text(result: &Value) -> String {
    let Some(blocks) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some(kind) => parts.push(format!("[{kind} returned]")),
            None => log::warn!("a tool result block has no type: {block}"),
        }
    }
    parts.join("\n")
}

/// A schema the model can use. Servers that describe no arguments send `null` or
/// nothing at all; both mean "this tool takes none".
fn input_schema(schema: Option<&Value>) -> Value {
    match schema {
        Some(Value::Object(map)) if !map.is_empty() => Value::Object(map.clone()),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// How a connection reaches its server. Stdio is the only one today; a URL would
/// sit behind the same seam.
trait Transport: Send {
    fn send(&mut self, message: &str) -> Result<()>;
    /// The next message as it arrived, or `None` when the server closed its end.
    fn receive(&mut self, timeout: Duration) -> Result<Option<String>>;
}

/// A server run as a child process, talking over its stdin and stdout.
struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Option<String>>,
}

impl StdioTransport {
    fn spawn(
        name: &str,
        program: &str,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Result<Self> {
        let mut command = Command::new(program);
        command
            .args(&args)
            .envs(env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start mcp server `{name}`"))?;
        let stdin = child.stdin.take().context("the server has no stdin")?;
        let stdout = child.stdout.take().context("the server has no stdout")?;
        let stderr = child.stderr.take();

        let (sender, messages) = mpsc::channel();
        let label = name.to_owned();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message = match line {
                    Ok(line) => Some(line),
                    Err(error) => {
                        log::warn!("mcp {label}: failed to read: {error}");
                        None
                    }
                };
                let done = message.is_none();
                if sender.send(message).is_err() || done {
                    return;
                }
            }
            // The server closed its output; say so, once. A failed send means the
            // client is gone, which is the same thing from here.
            sender.send(None).ok();
        });

        // A server that complains is the only diagnostic there is when a tool does
        // not appear, so its stderr goes to the log instead of a closed pipe.
        if let Some(stderr) = stderr {
            let label = name.to_owned();
            std::thread::spawn(move || {
                let mut lines = BufReader::new(stderr);
                let mut buffer = String::new();
                loop {
                    buffer.clear();
                    match lines.read_line(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => log::info!("mcp {label}: {}", buffer.trim_end()),
                    }
                }
            });
        }

        Ok(StdioTransport {
            child,
            stdin,
            messages,
        })
    }
}

impl Transport for StdioTransport {
    fn send(&mut self, message: &str) -> Result<()> {
        // MCP over stdio is one message per line, and the newline is the frame.
        writeln!(self.stdin, "{message}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<String>> {
        match self.messages.recv_timeout(timeout) {
            Ok(message) => Ok(message),
            Err(RecvTimeoutError::Timeout) => {
                bail!("no answer within {}s", timeout.as_secs_f32())
            }
            // The reader thread is gone, which it only is once the process is.
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            log::debug!("mcp {} was already gone: {error}", self.child.id());
        }
        if let Err(error) = self.child.wait() {
            log::debug!("mcp {} could not be reaped: {error}", self.child.id());
        }
    }
}

/// What came back for a message: an answer, or the reason there will not be one.
enum Incoming {
    Message(String),
    Failed(String),
}

/// A server reached over streamable HTTP: one POST per message, and either a JSON
/// answer or an event stream carrying it.
///
/// Each message is sent on its own thread, because the request that carries it may
/// stay open as a stream; the reader thread hands messages back through the same
/// channel the client waits on.
struct HttpTransport {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
    session: Arc<Mutex<Option<String>>>,
    incoming: Receiver<Incoming>,
    /// Kept so the channel never looks closed between two messages.
    sender: Sender<Incoming>,
}

impl HttpTransport {
    fn new(name: String, url: String, headers: Vec<(String, String)>) -> Self {
        let (sender, incoming) = mpsc::channel();
        HttpTransport {
            name,
            url,
            headers,
            session: Arc::new(Mutex::new(None)),
            incoming,
            sender,
        }
    }

    /// One client per process: it keeps the connection pool.
    fn client() -> Arc<dyn HttpClient> {
        static CLIENT: std::sync::OnceLock<Arc<dyn HttpClient>> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| Arc::new(reqwest_client::ReqwestClient::new()))
            .clone()
    }
}

impl Transport for HttpTransport {
    fn send(&mut self, message: &str) -> Result<()> {
        let parsed: Value = serde_json::from_str(message).context("a message is not JSON")?;
        let reply_expected = parsed.get("id").is_some();
        let initialize = parsed.get("method").and_then(Value::as_str) == Some("initialize");

        let name = self.name.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        let session = self.session.clone();
        let sender = self.sender.clone();
        let message = message.to_owned();

        std::thread::spawn(move || {
            let result = post(
                &url,
                &headers,
                &session,
                &sender,
                &message,
                reply_expected,
                initialize,
            );
            if let Err(error) = result {
                log::warn!("mcp {name}: {error:#}");
                sender
                    .send(Incoming::Failed(format!("the request failed: {error:#}")))
                    .ok();
            }
        });
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<String>> {
        match self.incoming.recv_timeout(timeout) {
            Ok(Incoming::Message(message)) => Ok(Some(message)),
            Ok(Incoming::Failed(reason)) => bail!("{reason}"),
            Err(RecvTimeoutError::Timeout) => bail!("no answer within {}s", timeout.as_secs_f32()),
            // The reader threads are gone with the transport; treat it as closed.
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

fn post(
    url: &str,
    headers: &[(String, String)],
    session: &Arc<Mutex<Option<String>>>,
    sender: &Sender<Incoming>,
    message: &str,
    reply_expected: bool,
    initialize: bool,
) -> Result<()> {
    let mut builder = Request::builder().method(Method::POST).uri(url);
    builder = builder
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    // The spec asks for the version on every request after the handshake; sending
    // it on the handshake too confuses servers that predate the header.
    if !initialize {
        builder = builder.header("MCP-Protocol-Version", PROTOCOL_VERSION);
    }
    if let Some(session) = session.lock().ok().and_then(|session| session.clone()) {
        builder = builder.header("Mcp-Session-Id", session);
    }
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(AsyncBody::from(message.to_owned()))
        .context("the request could not be built")?;

    let response = block_on(HttpTransport::client().send(request)).context("the request failed")?;

    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        && let Ok(mut session) = session.lock()
    {
        *session = Some(id.to_owned());
    }

    let status = response.status();
    if status.as_u16() == 202 {
        // A notification was accepted; nothing is coming back.
        return Ok(());
    }
    if !status.is_success() {
        let body = read_all(response.into_body());
        bail!("the server answered {status}: {}", body.trim());
    }

    let streaming = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut body = response.into_body();
    if streaming {
        stream_events(&mut body, sender, reply_expected)
    } else {
        let text = read_all(body);
        sender.send(Incoming::Message(text)).ok();
        Ok(())
    }
}

fn read_all(mut body: AsyncBody) -> String {
    let mut bytes = Vec::new();
    if let Err(error) = block_on(body.read_to_end(&mut bytes)) {
        log::warn!("a response body could not be read: {error}");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Reads server-sent events until the stream ends or the answer to this message has
/// arrived. The rest of a stream belongs to later messages, and this connection is
/// closed rather than held open for them.
fn stream_events(
    body: &mut AsyncBody,
    sender: &Sender<Incoming>,
    reply_expected: bool,
) -> Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = block_on(body.read(&mut chunk)).context("the stream could not be read")?;
        if read == 0 {
            return Ok(());
        }
        // Event framing is `\n\n`, and dropping the carriage returns makes a CRLF
        // stream and an LF stream the same thing. Filtering bytes, not text, keeps a
        // character split across two reads intact.
        pending.extend(chunk[..read].iter().copied().filter(|byte| *byte != b'\r'));

        while let Some(end) = find_blank_line(&pending) {
            let event: Vec<u8> = pending.drain(..end + 2).collect();
            let text = String::from_utf8_lossy(&event);
            let mut data = String::new();
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            if data.is_empty() {
                continue;
            }
            let answered = reply_expected
                && serde_json::from_str::<Value>(&data)
                    .ok()
                    .and_then(|value| value.get("id").cloned())
                    .is_some();
            if sender.send(Incoming::Message(data)).is_err() {
                return Ok(());
            }
            if answered {
                return Ok(());
            }
        }
    }
}

fn find_blank_line(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|pair| pair == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// The messages a test server was sent, and the answers it gives back.
    struct Scripted {
        sent: Arc<Mutex<Vec<Value>>>,
        replies: VecDeque<String>,
    }

    impl Scripted {
        fn new(replies: &[&str]) -> (Self, Arc<Mutex<Vec<Value>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let scripted = Scripted {
                sent: sent.clone(),
                replies: replies.iter().map(|reply| (*reply).to_owned()).collect(),
            };
            (scripted, sent)
        }
    }

    impl Transport for Scripted {
        fn send(&mut self, message: &str) -> Result<()> {
            self.sent
                .lock()
                .expect("sent")
                .push(serde_json::from_str(message)?);
            Ok(())
        }

        fn receive(&mut self, _timeout: Duration) -> Result<Option<String>> {
            Ok(self.replies.pop_front())
        }
    }

    fn initialized() -> &'static str {
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#
    }

    #[test]
    fn connecting_says_hello_and_then_says_it_is_ready() {
        let (transport, sent) = Scripted::new(&[initialized()]);
        let mut server = Server::with_transport("test", Box::new(transport), DEFAULT_TIMEOUT);
        server.handshake().expect("the handshake");

        let sent = sent.lock().expect("sent");
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[0]["params"]["clientInfo"]["name"], "humibird");
        assert_eq!(sent[1]["method"], "notifications/initialized");
        assert!(sent[1].get("id").is_none(), "a notification carries no id");
        assert_eq!(server.version(), Some("2025-06-18"));
    }

    #[test]
    fn tools_are_read_across_pages() {
        let (transport, sent) = Scripted::new(&[
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read","description":"Read a note","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}},{"name":"bare"}],"nextCursor":"page2"}}"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"write"}]}}"#,
        ]);
        let mut server = Server::with_transport("notes", Box::new(transport), DEFAULT_TIMEOUT);
        server.handshake().expect("the handshake");

        let tools = server.tools().expect("two pages");
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "bare", "write"]
        );
        assert_eq!(tools[0].description, "Read a note");
        assert_eq!(
            tools[0].input_schema["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(
            tools[1].input_schema,
            json!({"type": "object", "properties": {}}),
            "a tool that describes no arguments takes an empty object"
        );

        let sent = sent.lock().expect("sent");
        assert_eq!(sent[2]["method"], "tools/list");
        assert_eq!(sent[2]["params"], json!({}));
        assert_eq!(sent[3]["params"], json!({"cursor": "page2"}));
    }

    #[test]
    fn a_call_returns_what_the_server_said() {
        let (transport, sent) = Scripted::new(&[
            initialized(),
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"},{"type":"image","data":"","mimeType":"image/png"}]}}"#,
        ]);
        let mut server = Server::with_transport("notes", Box::new(transport), DEFAULT_TIMEOUT);
        server.handshake().expect("the handshake");

        let text = server
            .call("read", &json!({"path": "notes.md"}))
            .expect("the call");
        assert_eq!(text, "line one\nline two\n[image returned]");

        let sent = sent.lock().expect("sent");
        assert_eq!(sent[2]["method"], "tools/call");
        assert_eq!(sent[2]["params"]["name"], "read");
        assert_eq!(sent[2]["params"]["arguments"], json!({"path": "notes.md"}));
    }

    #[test]
    fn a_call_the_server_calls_an_error_is_an_error() {
        let (transport, _sent) = Scripted::new(&[
            initialized(),
            r#"{"jsonrpc":"2.0","id":2,"result":{"isError":true,"content":[{"type":"text","text":"no such note"}]}}"#,
        ]);
        let mut server = Server::with_transport("notes", Box::new(transport), DEFAULT_TIMEOUT);
        server.handshake().expect("the handshake");

        let error = server
            .call("read", &json!({}))
            .expect_err("the model has to be told");
        assert!(format!("{error:#}").contains("no such note"), "{error:#}");
    }

    #[test]
    fn a_notification_that_arrives_mid_request_is_skipped() {
        let (transport, _sent) = Scripted::new(&[
            initialized(),
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#,
        ]);
        let mut server = Server::with_transport("notes", Box::new(transport), DEFAULT_TIMEOUT);
        server.handshake().expect("the handshake");

        assert!(server.tools().expect("the list").is_empty());
    }

    #[test]
    fn a_server_that_stops_answering_is_not_used_again() {
        struct Silent;
        impl Transport for Silent {
            fn send(&mut self, _message: &str) -> Result<()> {
                Ok(())
            }
            fn receive(&mut self, _timeout: Duration) -> Result<Option<String>> {
                bail!("no answer within 0.02s")
            }
        }

        let mut server = Server::with_transport("notes", Box::new(Silent), DEFAULT_TIMEOUT);
        let error = server.tools().expect_err("a silent server fails");
        assert!(format!("{error:#}").contains("stopped"), "{error:#}");

        let again = server.tools().expect_err("and is not asked again");
        assert!(
            format!("{again:#}").contains("stopped answering"),
            "{again:#}"
        );
    }

    #[test]
    fn a_server_that_ends_its_output_is_reported() {
        let (transport, _sent) = Scripted::new(&[]);
        let mut server = Server::with_transport("notes", Box::new(transport), DEFAULT_TIMEOUT);
        let error = server.tools().expect_err("a closed server fails");
        assert!(
            format!("{error:#}").contains("closed its output"),
            "{error:#}"
        );
    }

    /// A reply the fake server gives: status, content type, body, extra headers.
    struct Reply {
        status: u16,
        content_type: &'static str,
        body: String,
        extra_headers: &'static str,
    }

    impl Reply {
        fn json(body: Value) -> Self {
            Reply {
                status: 200,
                content_type: "application/json",
                body: body.to_string(),
                extra_headers: "",
            }
        }

        fn events(body: String) -> Self {
            Reply {
                status: 200,
                content_type: "text/event-stream",
                body,
                extra_headers: "",
            }
        }

        fn accepted() -> Self {
            Reply {
                status: 202,
                content_type: "text/plain",
                body: String::new(),
                extra_headers: "",
            }
        }
    }

    /// A one-connection-per-request HTTP server: enough of the protocol to record
    /// what a client sent and to answer it.
    struct FakeHttp {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn fake_http(reply: impl Fn(&Value) -> Reply + Send + 'static) -> FakeHttp {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/mcp", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let remembered = requests.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some((head, body)) = read_request(&mut stream) else {
                    break;
                };
                let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let answer = reply(&value);
                remembered
                    .lock()
                    .expect("requests")
                    .push(format!("{head}\n\n{body}"));

                let response = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n{}\r\n{}",
                    answer.status,
                    answer.content_type,
                    answer.body.len(),
                    answer.extra_headers,
                    answer.body
                );
                stream.write_all(response.as_bytes()).ok();
                stream.flush().ok();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        FakeHttp { url, requests }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
        use std::io::{BufRead as _, BufReader, Read as _};

        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut head = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
            let done = line == "\r\n" || line == "\n";
            head.push_str(&line);
            if done {
                break;
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).ok()?;
        Some((head, String::from_utf8_lossy(&body).into_owned()))
    }

    fn handshake_reply(value: &Value) -> Reply {
        match value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "initialize" => Reply {
                extra_headers: "mcp-session-id: abc123\r\n",
                ..Reply::json(json!({
                    "jsonrpc": "2.0",
                    "id": value["id"].clone(),
                    "result": { "protocolVersion": PROTOCOL_VERSION, "capabilities": {} },
                }))
            },
            "notifications/initialized" => Reply::accepted(),
            "tools/list" => Reply::json(json!({
                "jsonrpc": "2.0",
                "id": value["id"].clone(),
                "result": { "tools": [{ "name": "ping", "description": "Ping" }] },
            })),
            "tools/call" => Reply::json(json!({
                "jsonrpc": "2.0",
                "id": value["id"].clone(),
                "result": { "content": [{ "type": "text", "text": "from http" }] },
            })),
            _ => Reply::json(json!({ "jsonrpc": "2.0", "id": value["id"].clone(), "result": {} })),
        }
    }

    #[test]
    fn an_http_server_is_greeted_and_asked_for_its_tools() {
        let http = fake_http(handshake_reply);
        let mut server = Server::connect("remote", &Options::http(http.url.clone()))
            .expect("the handshake over http");

        let tools = server.tools().expect("the tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");

        // Found by what they are, not by order: the initialized notification is not
        // waited on, so it may be posted after the request that follows it.
        let requests = http.requests.lock().expect("requests").clone();
        let request_with = |method: &str| {
            requests
                .iter()
                .find(|request| request.contains(&format!("\"method\":\"{method}\"")))
                .unwrap_or_else(|| panic!("no {method} in {requests:?}"))
        };
        let initialize = request_with("initialize");
        let listed = request_with("tools/list");

        assert!(
            initialize
                .to_ascii_lowercase()
                .contains("accept: application/json, text/event-stream"),
            "{initialize}"
        );
        assert!(
            !initialize
                .to_ascii_lowercase()
                .contains("mcp-protocol-version"),
            "the handshake does not carry the version header: {initialize}"
        );
        // The session id from the handshake reaches the next requests.
        assert!(
            listed
                .to_ascii_lowercase()
                .contains("mcp-session-id: abc123"),
            "{listed}"
        );
        assert!(
            listed
                .to_ascii_lowercase()
                .contains("mcp-protocol-version: 2025-06-18"),
            "{listed}"
        );
    }

    #[test]
    fn a_streamed_answer_is_read_as_a_message() {
        let http = fake_http(|value| match value.get("method").and_then(Value::as_str) {
            Some("tools/call") => Reply::events(format!(
                "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": {} }),
                json!({
                    "jsonrpc": "2.0",
                    "id": value["id"].clone(),
                    "result": { "content": [{ "type": "text", "text": "streamed" }] },
                })
            )),
            _ => handshake_reply(value),
        });

        let mut server =
            Server::connect("remote", &Options::http(http.url.clone())).expect("the handshake");
        let text = server.call("ping", &json!({})).expect("the call");
        assert_eq!(text, "streamed");
    }

    #[test]
    fn a_server_can_be_started_and_stopped() {
        // A real child process, so the framing, the reader thread and the kill on
        // drop are all exercised. It answers every line with the same idle reply;
        // the client only needs it to speak the protocol shape.
        let script = r#"
            while IFS= read -r line; do
                case "$line" in
                    *'"method":"initialize"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}'
                        ;;
                    *'"method":"tools/list"'*)
                        printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping"}]}}'
                        ;;
                esac
            done
        "#;
        let options = Options::stdio("sh")
            .args(vec!["-c".to_owned(), script.to_owned()])
            .timeout(Duration::from_secs(5));

        let mut server = Server::connect("sh", &options).expect("the server starts");
        let tools = server.tools().expect("it lists one tool");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");
    }
}
