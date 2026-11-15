//! Terminal startup: which server the app talks to, and whether it starts
//! one. The decision is pure ([`decide`]): the websocket probe, the unix
//! socket probe and the `modron` binary lookup are injected, so every branch
//! is testable. The I/O helpers ([`probe_ws`], [`probe_unix`], [`find_bin`],
//! [`ServerChild`]) are small and only the terminal binary uses them. Native
//! only (the web build never starts a process).

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use modron_mobile_core::{split_url_token, ConfigPatch, Effort, SandboxPolicy};
use modron_protocol::{ApprovalModeWire, DEFAULT_PORT};
use uuid::Uuid;

use crate::session::Resume;

/// How long one websocket probe may take (connect and reply).
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a server the app started may take to accept connections.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Time between SIGTERM and SIGKILL when the app stops its own server.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// The usage text printed by `--help` and after a bad argument.
pub const USAGE: &str =
    "usage: modron-app-tui [--demo | --url <ws url> | --socket <path>] [--token <t>]
                 [--session <uuid> | --new | --continue] [--provider <p>] [--model <m>]
                 [--keep-server] [--insecure-lan]
                 [--yolo] [--effort <low|medium|high|xhigh|max>] [--goal <text>]
                 [--agent <name>] [--sandbox <off|seatbelt|bwrap>]
                 [--allow <tool>]... [--deny <tool>]...
                 [--working-dir <dir>] [--sessions-dir <dir>]
  no flags: the unix socket (~/.modron/serve.sock) when it answers, else the
            server at MODRON_URL or ws://127.0.0.1:7699; if nothing answers
            there, start `modron serve` on that port
  --resume <id> is --session; --approve-all is --yolo
  --yolo, --effort, --goal, --agent, --sandbox, --allow and --deny set the
            attached session's config (future turns only)
  --provider, --model, --working-dir and --sessions-dir go to a server the
            app starts; they are refused when attaching to a running one
  --insecure-lan allows plain ws:// to a non-loopback host (trusted LAN only)
  env: MODRON_URL (server url), MODRON_BIN (the modron binary),
       MODRON_DEVICE_TOKEN (device token, when --token is not given)
  log of a server the app starts: $XDG_STATE_HOME/modron/app-serve.log
               (or ~/.local/state/modron/app-serve.log)";

/// The parsed command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub demo: bool,
    pub url: Option<String>,
    pub socket: Option<String>,
    pub token: Option<String>,
    pub session: Option<Uuid>,
    pub new: bool,
    pub cont: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub keep_server: bool,
    pub insecure_lan: bool,
    pub yolo: bool,
    pub effort: Option<Effort>,
    pub goal: Option<String>,
    pub agent: Option<String>,
    pub sandbox: Option<SandboxPolicy>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub working_dir: Option<String>,
    pub sessions_dir: Option<String>,
    pub help: bool,
}

/// The effort level a word names (the `--effort` values).
pub fn effort_word(word: &str) -> Option<Effort> {
    match word {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::Xhigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// The sandbox policy a word names (the `--sandbox` values).
pub fn sandbox_word(word: &str) -> Option<SandboxPolicy> {
    match word {
        "off" => Some(SandboxPolicy::Off),
        "seatbelt" => Some(SandboxPolicy::Seatbelt),
        "bwrap" | "bubblewrap" => Some(SandboxPolicy::Bubblewrap),
        _ => None,
    }
}

/// Parses the flags. Value flags take the next argument; `--allow` and
/// `--deny` repeat.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--demo" => out.demo = true,
            "--new" => out.new = true,
            "--continue" => out.cont = true,
            "--keep-server" => out.keep_server = true,
            "--insecure-lan" => out.insecure_lan = true,
            "--yolo" | "--approve-all" => out.yolo = true,
            "-h" | "--help" => out.help = true,
            "--url" => out.url = Some(value(&mut it, &flag)?),
            "--socket" => out.socket = Some(value(&mut it, &flag)?),
            "--token" => out.token = Some(value(&mut it, &flag)?),
            "--provider" => out.provider = Some(value(&mut it, &flag)?),
            "--model" => out.model = Some(value(&mut it, &flag)?),
            "--goal" => out.goal = Some(value(&mut it, &flag)?),
            "--agent" => out.agent = Some(value(&mut it, &flag)?),
            "--allow" => out.allow.push(value(&mut it, &flag)?),
            "--deny" => out.deny.push(value(&mut it, &flag)?),
            "--working-dir" => out.working_dir = Some(value(&mut it, &flag)?),
            "--sessions-dir" => out.sessions_dir = Some(value(&mut it, &flag)?),
            "--effort" => {
                let word = value(&mut it, &flag)?;
                out.effort =
                    Some(effort_word(&word).ok_or_else(|| {
                        "--effort: use low, medium, high, xhigh or max".to_string()
                    })?);
            }
            "--sandbox" => {
                let word = value(&mut it, &flag)?;
                out.sandbox = Some(
                    sandbox_word(&word)
                        .ok_or_else(|| "--sandbox: use off, seatbelt or bwrap".to_string())?,
                );
            }
            "--session" | "--resume" => {
                let text = value(&mut it, &flag)?;
                out.session = Some(Uuid::parse_str(&text).map_err(|e| format!("{flag}: {e}"))?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if out.new && out.cont {
        return Err("--new and --continue are exclusive".into());
    }
    if (out.new || out.cont) && out.session.is_some() {
        return Err("--session is exclusive with --new and --continue".into());
    }
    if out.demo && out.url.is_some() {
        return Err("--demo and --url are exclusive".into());
    }
    if out.url.is_some() && out.socket.is_some() {
        return Err("--url and --socket are exclusive".into());
    }
    Ok(out)
}

fn value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

/// Which session to attach to when no `--session` is given.
pub fn resume_of(args: &Args) -> Resume {
    if args.new {
        Resume::New
    } else if args.cont {
        Resume::Continue
    } else {
        Resume::Latest
    }
}

/// The config the flags ask for, in the order they are sent after attach:
/// approval, sandbox, effort, goal, persona, then the tool lists.
pub fn startup_patches(args: &Args) -> Vec<ConfigPatch> {
    let mut out = Vec::new();
    if args.yolo {
        out.push(ConfigPatch {
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            ..ConfigPatch::default()
        });
    }
    if let Some(sandbox) = args.sandbox {
        out.push(ConfigPatch { sandbox: Some(sandbox), ..ConfigPatch::default() });
    }
    if let Some(effort) = args.effort {
        out.push(ConfigPatch { effort: Some(effort), ..ConfigPatch::default() });
    }
    if let Some(goal) = &args.goal {
        out.push(ConfigPatch { goal: Some(goal.clone()), ..ConfigPatch::default() });
    }
    if let Some(agent) = &args.agent {
        out.push(ConfigPatch { persona: Some(agent.clone()), ..ConfigPatch::default() });
    }
    if !args.allow.is_empty() || !args.deny.is_empty() {
        out.push(ConfigPatch {
            allow: (!args.allow.is_empty()).then(|| args.allow.clone()),
            deny: (!args.deny.is_empty()).then(|| args.deny.clone()),
            ..ConfigPatch::default()
        });
    }
    out
}

/// The device token to authenticate with: `--token`, else the URL's
/// `?token=`, else `MODRON_DEVICE_TOKEN`. Empty values count as absent.
pub fn resolve_token(
    explicit: Option<&str>,
    url: Option<&str>,
    device_env: Option<&str>,
) -> Option<String> {
    explicit
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| url.and_then(|u| split_url_token(u).1))
        .or_else(|| device_env.filter(|t| !t.is_empty()).map(str::to_string))
}

/// Where the closed-over startup inputs come from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    /// `MODRON_URL`, when set and not empty.
    pub url: Option<String>,
    /// The default unix socket (`~/.modron/serve.sock`).
    pub socket: PathBuf,
}

/// Where the connection's server came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `--url` or `--socket` was given.
    Given,
    /// Nothing was given: a server already answered at the default address.
    Running,
}

/// What the terminal host does at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The built-in demo script.
    Demo,
    /// Connect to a server that is (or may be) already running. `url` is a
    /// ws(s) address with any `?token=` removed, or `unix:<path>`.
    Connect { url: String, source: Source },
    /// Start `modron serve --listen <listen>` as our child, then connect.
    Spawn { url: String, listen: String, bin: PathBuf },
    /// Nothing to connect to and no way to start it. `reason` is shown.
    Fail { reason: String },
}

/// The local server address when neither `--url` nor `MODRON_URL` is set.
pub fn default_url() -> String {
    format!("ws://127.0.0.1:{DEFAULT_PORT}")
}

/// The startup order: `--demo`; `--socket` and `--url` (connect, no probe);
/// the default unix socket when `MODRON_URL` is unset and it answers; the
/// websocket address (`MODRON_URL`, else the default) when it answers;
/// otherwise start a server on that port, when the address is local and a
/// binary exists. Flags that only a started server can take (`--working-dir`,
/// `--sessions-dir`) refuse every connect plan.
pub fn decide(
    args: &Args,
    env: &Env,
    probe_ws: impl Fn(&str) -> bool,
    probe_unix: impl Fn(&Path) -> bool,
    find_bin: impl FnOnce() -> Option<PathBuf>,
) -> Plan {
    if args.demo {
        return Plan::Demo;
    }
    if let Some(socket) = &args.socket {
        return connect_plan(args, format!("unix:{socket}"), Source::Given);
    }
    if let Some(url) = &args.url {
        return connect_plan(args, url.clone(), Source::Given);
    }
    if env.url.is_none() && probe_unix(&env.socket) {
        let url = format!("unix:{}", env.socket.display());
        return connect_plan(args, url, Source::Running);
    }
    let raw = env.url.clone().unwrap_or_else(default_url);
    let (url, _) = split_url_token(&raw);
    if probe_ws(&url) {
        return connect_plan(args, url, Source::Running);
    }
    let Some(endpoint) = WsEndpoint::parse(&url) else {
        return Plan::Fail { reason: format!("MODRON_URL is not a ws://host:port address: {url}") };
    };
    if !endpoint.is_local() {
        return Plan::Fail {
            reason: format!(
                "nothing answers at {url}, and modron-app only starts a server on this machine"
            ),
        };
    }
    match find_bin() {
        Some(bin) => Plan::Spawn { listen: endpoint.listen_addr(), url, bin },
        None => Plan::Fail {
            reason: format!(
                "nothing answers at {url}, and no modron binary was found \
                 (looked at MODRON_BIN, next to this app, and on PATH)"
            ),
        },
    }
}

/// A connect plan, or the reason it is refused: a flag that needs a started
/// server, or a plain `ws://` address off the loopback without `--insecure-lan`.
fn connect_plan(args: &Args, url: String, source: Source) -> Plan {
    if args.working_dir.is_some() || args.sessions_dir.is_some() {
        return Plan::Fail {
            reason: format!(
                "--working-dir and --sessions-dir apply only when modron-app starts the \
                 server; this run would attach to the server at {url}"
            ),
        };
    }
    if let Some(endpoint) = WsEndpoint::parse(&url) {
        if !endpoint.is_local() && !args.insecure_lan {
            return Plan::Fail {
                reason: format!(
                    "{url} is plain ws:// to a non-loopback host; pass --insecure-lan to \
                     allow it (trusted LAN only)"
                ),
            };
        }
    }
    Plan::Connect { url, source }
}

/// The binary lookup: `MODRON_BIN` when set (and it must be a file: no
/// silent fallback), else `modron` next to the app's executable, else
/// `modron` on `PATH`.
pub fn find_bin(
    env_bin: Option<&str>,
    exe_dir: Option<&Path>,
    path_var: Option<&str>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(bin) = env_bin.filter(|s| !s.is_empty()) {
        let bin = PathBuf::from(bin);
        return is_file(&bin).then_some(bin);
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join("modron");
        if is_file(&candidate) {
            return Some(candidate);
        }
    }
    path_var?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("modron"))
        .find(|candidate| is_file(candidate))
}

/// The `ws://host:port/path` parts of a server address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsEndpoint {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl WsEndpoint {
    /// Parses `ws://host[:port][/path]`. The port defaults to
    /// [`DEFAULT_PORT`]. Other schemes give `None`.
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("ws://")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (hostport, DEFAULT_PORT),
        };
        (!host.is_empty()).then(|| Self { host: host.into(), port, path: path.into() })
    }

    /// Loopback only: the app starts servers on this machine, nowhere else.
    pub fn is_local(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
    }

    /// The bind address for a server the app starts.
    pub fn listen_addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

/// True when a unix socket accepts a connection at `path` (the default
/// socket of the old terminal client, `--socket`).
#[cfg(unix)]
pub fn probe_unix(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// No unix sockets off unix: the websocket address is the only probe.
#[cfg(not(unix))]
pub fn probe_unix(_path: &Path) -> bool {
    false
}

/// True when the address answers a websocket upgrade with `101` within
/// [`PROBE_TIMEOUT`]. Plain TCP, no Origin header, so a server that refuses
/// browser origins still answers.
pub fn probe_ws(url: &str) -> bool {
    let Some(endpoint) = WsEndpoint::parse(url) else {
        return false;
    };
    let target = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next());
    let Some(target) = target else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&target, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        endpoint.path, endpoint.host, endpoint.port
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while head.len() < 12 {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    head.starts_with(b"HTTP/1.1 101")
}

/// Where the log of a server the app starts goes:
/// `$XDG_STATE_HOME/modron/app-serve.log`, else `~/.local/state/modron/`.
pub fn log_path(xdg_state_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let dir = match (xdg_state_home.filter(|s| !s.is_empty()), home.filter(|s| !s.is_empty())) {
        (Some(xdg), _) => PathBuf::from(xdg).join("modron"),
        (None, Some(home)) => PathBuf::from(home).join(".local/state/modron"),
        (None, None) => std::env::temp_dir().join("modron"),
    };
    dir.join("app-serve.log")
}

/// How to start a server: the binary, the bind address and the flags passed
/// through. `dir` and `home` override the working directory and `HOME` of
/// the server (tests use them; the app inherits both).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerSpec {
    pub bin: PathBuf,
    pub listen: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub log: PathBuf,
    pub dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// `--working-dir` and `--sessions-dir` of the server (its own flags).
    pub working_dir: Option<String>,
    pub sessions_dir: Option<String>,
}

/// A `modron serve` the app started. It is the app's child: dropping it
/// stops the server (SIGTERM, then SIGKILL after two seconds) unless
/// [`ServerChild::detach`] was called (`--keep-server`).
pub struct ServerChild {
    child: Child,
    keep: bool,
}

impl ServerChild {
    /// Spawns the server with stdout and stderr in the log file, then waits
    /// up to `timeout` for the port to accept connections. A server that
    /// exits first, or never listens, is an error (the child is stopped).
    pub fn start(spec: &ServerSpec, timeout: Duration) -> Result<Self, String> {
        if let Some(dir) = spec.log.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let open = || {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&spec.log)
                .map_err(|e| format!("cannot open {}: {e}", spec.log.display()))
        };
        let (out, err) = (open()?, open()?);
        let mut cmd = Command::new(&spec.bin);
        cmd.arg("serve").args(["--listen", &spec.listen]);
        if let Some(provider) = &spec.provider {
            cmd.args(["--provider", provider]);
        }
        if let Some(model) = &spec.model {
            cmd.args(["--model", model]);
        }
        if let Some(dir) = &spec.working_dir {
            cmd.args(["--working-dir", dir]);
        }
        if let Some(dir) = &spec.sessions_dir {
            cmd.args(["--sessions-dir", dir]);
        }
        if let Some(dir) = &spec.dir {
            cmd.current_dir(dir);
        }
        if let Some(home) = &spec.home {
            cmd.env("HOME", home);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err));
        let child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", spec.bin.display()))?;
        let mut server = Self { child, keep: false };
        let addr: SocketAddr =
            spec.listen.parse().map_err(|e| format!("bad listen address {}: {e}", spec.listen))?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = server.child.try_wait() {
                return Err(format!("modron serve exited ({status}); see {}", spec.log.display()));
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                return Ok(server);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "modron serve did not listen on {} within {}s; see {}",
                    spec.listen,
                    timeout.as_secs(),
                    spec.log.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The server's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Leaves the server running when the app exits (`--keep-server`).
    pub fn detach(&mut self) {
        self.keep = true;
    }
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        if !self.keep {
            terminate(&mut self.child);
        }
    }
}

/// SIGTERM, then SIGKILL once [`KILL_GRACE`] has passed. Always reaps.
fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn args() -> Args {
        Args::default()
    }

    fn env(url: Option<&str>) -> Env {
        Env { url: url.map(str::to_string), socket: PathBuf::from("/home/u/.modron/serve.sock") }
    }

    fn no_probe(_: &str) -> bool {
        panic!("the websocket probe must not run here")
    }

    fn no_socket(_: &Path) -> bool {
        panic!("the socket probe must not run here")
    }

    fn no_bin() -> Option<PathBuf> {
        panic!("the binary lookup must not run here")
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn demo_flag_wins_over_everything() {
        let a = Args { demo: true, ..args() };
        assert_eq!(
            decide(&a, &env(Some("ws://127.0.0.1:9")), no_probe, no_socket, no_bin),
            Plan::Demo
        );
    }

    #[test]
    fn url_flag_connects_without_probing() {
        let a = Args { url: Some("ws://example.test:7000".into()), insecure_lan: true, ..args() };
        assert_eq!(
            decide(&a, &env(Some("ws://127.0.0.1:9")), no_probe, no_socket, no_bin),
            Plan::Connect { url: "ws://example.test:7000".into(), source: Source::Given }
        );
    }

    #[test]
    fn socket_flag_connects_to_that_unix_socket_without_probing() {
        let a = Args { socket: Some("/run/m.sock".into()), ..args() };
        assert_eq!(
            decide(&a, &env(None), no_probe, no_socket, no_bin),
            Plan::Connect { url: "unix:/run/m.sock".into(), source: Source::Given }
        );
    }

    #[test]
    fn a_plain_ws_url_off_the_loopback_needs_insecure_lan() {
        let a = Args { url: Some("ws://10.0.0.5:7699".into()), ..args() };
        match decide(&a, &env(None), no_probe, no_socket, no_bin) {
            Plan::Fail { reason } => assert!(reason.contains("--insecure-lan"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let lan = Args { insecure_lan: true, ..a };
        assert_eq!(
            decide(&lan, &env(None), no_probe, no_socket, no_bin),
            Plan::Connect { url: "ws://10.0.0.5:7699".into(), source: Source::Given }
        );
    }

    #[test]
    fn a_running_unix_socket_is_preferred_when_no_url_is_set() {
        let socket = PathBuf::from("/home/u/.modron/serve.sock");
        let plan = decide(&args(), &env(None), no_probe, |p| p == socket, no_bin);
        assert_eq!(
            plan,
            Plan::Connect {
                url: "unix:/home/u/.modron/serve.sock".into(),
                source: Source::Running
            }
        );
    }

    #[test]
    fn an_explicit_env_url_skips_the_socket_and_falls_back_to_the_probe() {
        let plan = decide(
            &args(),
            &env(Some("ws://127.0.0.1:7123")),
            |u| u.ends_with(":7123"),
            |_| panic!("no socket probe"),
            no_bin,
        );
        assert_eq!(
            plan,
            Plan::Connect { url: "ws://127.0.0.1:7123".into(), source: Source::Running }
        );
    }

    #[test]
    fn a_silent_socket_falls_back_to_the_websocket_probe() {
        let plan = decide(&args(), &env(None), |u| u == default_url(), |_| false, no_bin);
        assert_eq!(plan, Plan::Connect { url: default_url(), source: Source::Running });
    }

    #[test]
    fn a_token_in_the_env_url_is_not_probed_or_kept() {
        let plan = decide(
            &args(),
            &env(Some("ws://127.0.0.1:7125?token=abc")),
            |u| u == "ws://127.0.0.1:7125",
            |_| false,
            no_bin,
        );
        assert_eq!(
            plan,
            Plan::Connect { url: "ws://127.0.0.1:7125".into(), source: Source::Running }
        );
    }

    #[test]
    fn nothing_answering_and_a_binary_spawns_on_the_port() {
        let bin = PathBuf::from("/opt/modron");
        let plan = decide(
            &args(),
            &env(Some("ws://localhost:7124")),
            |_| false,
            |_| false,
            || Some(bin.clone()),
        );
        assert_eq!(
            plan,
            Plan::Spawn { url: "ws://localhost:7124".into(), listen: "127.0.0.1:7124".into(), bin }
        );
    }

    #[test]
    fn nothing_answering_and_no_binary_fails_with_a_reason() {
        match decide(&args(), &env(None), |_| false, |_| false, || None) {
            Plan::Fail { reason } => assert!(reason.contains("no modron binary"), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_remote_address_is_never_started_locally() {
        match decide(&args(), &env(Some("ws://10.1.2.3:7699")), |_| false, |_| false, no_bin) {
            Plan::Fail { reason } => assert!(reason.contains("only starts"), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_non_ws_env_url_fails_with_a_reason() {
        match decide(&args(), &env(Some("https://127.0.0.1:7699")), |_| false, |_| false, no_bin) {
            Plan::Fail { reason } => assert!(reason.contains("MODRON_URL"), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn working_and_sessions_dirs_are_refused_when_attaching() {
        let a = Args { working_dir: Some("/w".into()), ..args() };
        match decide(&a, &env(None), |_| true, |_| false, no_bin) {
            Plan::Fail { reason } => assert!(reason.contains("--working-dir"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let b = Args {
            sessions_dir: Some("/s".into()),
            url: Some("ws://127.0.0.1:1".into()),
            ..args()
        };
        assert!(matches!(decide(&b, &env(None), no_probe, no_socket, no_bin), Plan::Fail { .. }));
    }

    #[test]
    fn working_dir_reaches_a_server_the_app_starts() {
        let a = Args { working_dir: Some("/w".into()), ..args() };
        let plan = decide(
            &a,
            &env(Some("ws://127.0.0.1:7126")),
            |_| false,
            |_| false,
            || Some(PathBuf::from("/opt/modron")),
        );
        assert!(matches!(plan, Plan::Spawn { .. }), "{plan:?}");
    }

    #[test]
    fn env_bin_is_used_only_when_it_is_a_file() {
        let is_file = |p: &Path| p == Path::new("/x/modron");
        assert_eq!(find_bin(Some("/x/modron"), None, None, is_file), Some("/x/modron".into()));
        // Set but missing: no fallback to PATH.
        assert_eq!(find_bin(Some("/y/modron"), None, Some("/x"), is_file), None);
    }

    #[test]
    fn exe_dir_beats_path_and_path_is_searched_in_order() {
        let is_file = |p: &Path| p == Path::new("/app/modron") || p == Path::new("/b/modron");
        assert_eq!(
            find_bin(None, Some(Path::new("/app")), Some("/a:/b"), is_file),
            Some("/app/modron".into())
        );
        assert_eq!(
            find_bin(None, Some(Path::new("/z")), Some("/a::/b"), is_file),
            Some("/b/modron".into())
        );
        assert_eq!(find_bin(None, None, None, is_file), None);
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            WsEndpoint::parse("ws://127.0.0.1:7000/x"),
            Some(WsEndpoint { host: "127.0.0.1".into(), port: 7000, path: "/x".into() })
        );
        assert_eq!(WsEndpoint::parse("ws://localhost").map(|e| e.port), Some(DEFAULT_PORT));
        assert_eq!(WsEndpoint::parse("wss://localhost:1"), None);
        assert_eq!(WsEndpoint::parse("ws://:1"), None);
    }

    #[test]
    fn args_parse_and_reject_conflicts() {
        let a = parse_args(
            ["--url", "ws://h:1", "--token", "t", "--provider", "mock", "--keep-server"]
                .map(String::from),
        )
        .expect("parse");
        assert_eq!(a.url.as_deref(), Some("ws://h:1"));
        assert_eq!(a.provider.as_deref(), Some("mock"));
        assert!(a.keep_server);
        assert!(parse_args(["--new", "--continue"].map(String::from)).is_err());
        assert!(
            parse_args(["--new", "--session", &Uuid::nil().to_string()].map(String::from)).is_err()
        );
        assert!(parse_args(["--demo", "--url", "ws://h:1"].map(String::from)).is_err());
        assert!(parse_args(["--url", "ws://h:1", "--socket", "/s"].map(String::from)).is_err());
        assert!(parse_args(["--url"].map(String::from)).is_err());
        assert!(parse_args(["--bogus"].map(String::from)).is_err());
        assert_eq!(resume_of(&parse_args(Vec::new()).unwrap()), Resume::Latest);
        assert_eq!(resume_of(&parse_args(["--continue".to_string()]).unwrap()), Resume::Continue);
        assert_eq!(resume_of(&parse_args(["--new".to_string()]).unwrap()), Resume::New);
    }

    #[test]
    fn the_config_flags_parse_into_their_fields_table_driven() {
        let id = Uuid::from_u128(7);
        type FlagCase = (Vec<&'static str>, Box<dyn Fn(&Args) -> bool>);
        let cases: Vec<FlagCase> = vec![
            (vec!["--yolo"], Box::new(|a: &Args| a.yolo)),
            (vec!["--approve-all"], Box::new(|a: &Args| a.yolo)),
            (vec!["--effort", "max"], Box::new(|a: &Args| a.effort == Some(Effort::Max))),
            (
                vec!["--sandbox", "bwrap"],
                Box::new(|a: &Args| a.sandbox == Some(SandboxPolicy::Bubblewrap)),
            ),
            (vec!["--goal", "ship it"], Box::new(|a: &Args| a.goal.as_deref() == Some("ship it"))),
            (
                vec!["--agent", "reviewer"],
                Box::new(|a: &Args| a.agent.as_deref() == Some("reviewer")),
            ),
            (
                vec!["--allow", "bash", "--allow", "read"],
                Box::new(|a: &Args| a.allow == strings(&["bash", "read"])),
            ),
            (vec!["--deny", "bash"], Box::new(|a: &Args| a.deny == strings(&["bash"]))),
            (
                vec!["--working-dir", "/w", "--sessions-dir", "/s"],
                Box::new(|a: &Args| {
                    a.working_dir.as_deref() == Some("/w")
                        && a.sessions_dir.as_deref() == Some("/s")
                }),
            ),
            (vec!["--insecure-lan"], Box::new(|a: &Args| a.insecure_lan)),
        ];
        for (flags, check) in cases {
            let parsed = parse_args(flags.iter().map(|s| s.to_string())).expect("parse");
            assert!(check(&parsed), "{flags:?} -> {parsed:?}");
        }
        let resumed = parse_args(["--resume".to_string(), id.to_string()]).expect("resume");
        assert_eq!(resumed.session, Some(id));
    }

    #[test]
    fn bad_config_values_and_unknown_flags_are_errors_with_a_reason() {
        for (flags, want) in [
            (vec!["--effort", "huge"], "--effort"),
            (vec!["--sandbox", "jail"], "--sandbox"),
            (vec!["--allow"], "needs a value"),
            (vec!["--frobnicate"], "unknown argument --frobnicate"),
        ] {
            let err = parse_args(flags.iter().map(|s| s.to_string())).expect_err("error");
            assert!(err.contains(want), "{flags:?}: {err}");
        }
    }

    #[test]
    fn startup_patches_follow_the_flag_order() {
        let a = Args {
            yolo: true,
            sandbox: Some(SandboxPolicy::Off),
            effort: Some(Effort::High),
            goal: Some("g".into()),
            agent: Some("reviewer".into()),
            allow: strings(&["read"]),
            ..args()
        };
        let patches = startup_patches(&a);
        assert_eq!(patches.len(), 6);
        assert_eq!(patches[0].approval_mode, Some(ApprovalModeWire::ApproveAll));
        assert_eq!(patches[1].sandbox, Some(SandboxPolicy::Off));
        assert_eq!(patches[2].effort, Some(Effort::High));
        assert_eq!(patches[3].goal.as_deref(), Some("g"));
        assert_eq!(patches[4].persona.as_deref(), Some("reviewer"));
        assert_eq!(patches[5].allow, Some(strings(&["read"])));
        assert_eq!(patches[5].deny, None, "an unset list is left alone");
        assert!(startup_patches(&args()).is_empty());
    }

    #[test]
    fn the_token_comes_from_the_flag_then_the_url_then_the_env() {
        assert_eq!(resolve_token(Some("a"), Some("ws://h?token=b"), Some("c")), Some("a".into()));
        assert_eq!(resolve_token(None, Some("ws://h?token=b"), Some("c")), Some("b".into()));
        assert_eq!(resolve_token(None, Some("ws://h"), Some("c")), Some("c".into()));
        assert_eq!(resolve_token(Some(""), None, Some("")), None);
        assert_eq!(resolve_token(None, None, None), None);
    }

    #[test]
    fn log_path_prefers_xdg_then_home() {
        assert_eq!(log_path(Some("/s"), Some("/h")), PathBuf::from("/s/modron/app-serve.log"));
        assert_eq!(
            log_path(Some(""), Some("/h")),
            PathBuf::from("/h/.local/state/modron/app-serve.log")
        );
    }

    fn fake_server(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(reply);
            }
        });
        format!("ws://127.0.0.1:{port}")
    }

    #[test]
    fn probe_accepts_a_websocket_upgrade_only() {
        assert!(probe_ws(&fake_server(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")));
        assert!(!probe_ws(&fake_server(b"HTTP/1.1 200 OK\r\n\r\n")));
        let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        assert!(!probe_ws(&format!("ws://127.0.0.1:{port}")));
    }
}
