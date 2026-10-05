//! MCP servers, defined once in unharness's config and handed to each
//! harness for the session. How a harness takes them is its own business
//! (`src/harness/<name>/`); nothing is written to a vendor's configuration.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::caps::{McpChannel, McpSupport};

/// One `[mcp_servers.<name>]` table: a `command` unharness's harnesses
/// launch (with `args` and `env`), or the `url` of a server reached over
/// HTTP (with `headers`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct McpServerSettings {
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// `false` keeps the definition but passes it to no harness. Default: on.
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub transport: McpTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl McpServer {
    pub fn is_http(&self) -> bool {
        matches!(self.transport, McpTransport::Http { .. })
    }

    /// The command line or the URL, for display.
    pub fn target(&self) -> String {
        match &self.transport {
            McpTransport::Stdio { command, args, .. } => {
                let mut words = vec![command.as_str()];
                words.extend(args.iter().map(String::as_str));
                words.join(" ")
            }
            McpTransport::Http { url, .. } => url.clone(),
        }
    }
}

/// The enabled servers of a config table, by name, and what is wrong with
/// the entries that were left out.
pub fn resolve(table: &BTreeMap<String, McpServerSettings>) -> (Vec<McpServer>, Vec<String>) {
    let (mut servers, mut problems) = (Vec::new(), Vec::new());
    for (name, s) in table {
        if s.enabled == Some(false) {
            continue;
        }
        let mut problem = |what: &str| problems.push(format!("MCP server '{name}' {what}"));
        // The name ends up in flags, config keys and tool names.
        let plain = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if name.is_empty() || !name.chars().all(plain) {
            problem("is ignored: a name may only have letters, digits, '-' and '_'");
            continue;
        }
        let transport = match (&s.command, &s.url) {
            (Some(_), Some(_)) => {
                problem("is ignored: it has both a command and a url");
                continue;
            }
            (None, None) => {
                problem("is ignored: it has neither a command nor a url");
                continue;
            }
            (Some(command), None) => {
                if !s.headers.is_empty() {
                    problem("has headers, which only a url server takes");
                }
                McpTransport::Stdio {
                    command: command.clone(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                }
            }
            (None, Some(url)) => {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    problem("is ignored: its url is not http(s)");
                    continue;
                }
                if !s.args.is_empty() || !s.env.is_empty() {
                    problem("has args or env, which only a command server takes");
                }
                McpTransport::Http {
                    url: url.clone(),
                    headers: s.headers.clone(),
                }
            }
        };
        servers.push(McpServer {
            name: name.clone(),
            transport,
        });
    }
    (servers, problems)
}

/// The servers to hand a harness with this `support`, and what to tell the
/// user about the ones it will not have.
///
/// A harness that takes servers over its protocol gets all of them: whether
/// it takes http ones is only known once its session answers, so its
/// transport drops and reports those itself.
pub fn for_harness(
    servers: &[McpServer],
    support: McpSupport,
    harness: &str,
) -> (Vec<McpServer>, Option<String>) {
    let names = |list: &[&McpServer]| -> String {
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        names.join(", ")
    };
    let Some(channel) = support.channel else {
        let all: Vec<&McpServer> = servers.iter().collect();
        let warning = (!all.is_empty()).then(|| {
            format!(
                "{harness} takes no MCP servers from unharness, only from its own \
                 configuration; not passed on: {}",
                names(&all)
            )
        });
        return (Vec::new(), warning);
    };
    if support.http || channel == McpChannel::Protocol {
        return (servers.to_vec(), None);
    }
    let (kept, dropped): (Vec<&McpServer>, Vec<&McpServer>) =
        servers.iter().partition(|s| !s.is_http());
    let warning = (!dropped.is_empty()).then(|| not_http(harness, &names(&dropped)));
    (kept.into_iter().cloned().collect(), warning)
}

/// The warning for http servers a harness does not take.
pub fn not_http(harness: &str, names: &str) -> String {
    format!("{harness} only takes MCP servers it launches itself; not passed on: {names}")
}

/// What a server said about itself when asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProbe {
    /// `serverInfo` name and version.
    pub server: String,
    pub tools: usize,
}

/// Start a command server, do the MCP handshake, count its tools and stop
/// it again. For `doctor`: it runs outside the sandbox, like the other
/// probes.
pub fn probe_stdio(server: &McpServer, timeout: Duration) -> Result<McpProbe, String> {
    let McpTransport::Stdio { command, args, env } = &server.transport else {
        return Err("not a command server".into());
    };
    let mut child = Command::new(command)
        .args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start '{command}': {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let call = |stdin: &mut ChildStdin, id: u64, method: &str, params: Value| {
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{request}").map_err(|e| format!("it closed its input: {e}"))?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = rx.recv_timeout(left).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => {
                    format!("no answer to {method} within {}s", timeout.as_secs())
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    format!("it exited before answering {method}")
                }
            })?;
            // Anything else on stdout is a notification or a log line.
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return match msg.get("error") {
                Some(e) => Err(format!(
                    "{method} failed: {}",
                    e.get("message").and_then(Value::as_str).unwrap_or("error")
                )),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
        }
    };
    let mut handshake = || -> Result<McpProbe, String> {
        let init = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "unharness", "version": env!("CARGO_PKG_VERSION")},
        });
        let info = call(&mut stdin, 1, "initialize", init)?;
        let text = |key: &str| {
            info.pointer(&format!("/serverInfo/{key}"))
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        let server = format!("{} {}", text("name"), text("version"))
            .trim()
            .to_string();
        let mut tools = 0;
        if info.pointer("/capabilities/tools").is_some() {
            let ready = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
            writeln!(stdin, "{ready}").map_err(|e| format!("it closed its input: {e}"))?;
            tools = call(&mut stdin, 2, "tools/list", json!({}))?
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
        }
        Ok(McpProbe { server, tools })
    };
    let outcome = handshake();
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(toml_str: &str) -> BTreeMap<String, McpServerSettings> {
        toml::from_str(toml_str).unwrap()
    }

    fn stdio(name: &str, command: &str, args: &[&str]) -> McpServer {
        McpServer {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: BTreeMap::new(),
            },
        }
    }

    fn http(name: &str) -> McpServer {
        McpServer {
            name: name.into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn a_table_resolves_to_command_and_url_servers() {
        let (servers, problems) = resolve(&table(
            r#"
[files]
command = "npx"
args = ["-y", "server-filesystem", "/work"]
env = { TOKEN = "t" }

[docs]
url = "https://example.com/mcp"
headers = { Authorization = "Bearer x" }

[off]
command = "never"
enabled = false
"#,
        ));
        assert!(problems.is_empty(), "{problems:?}");
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["docs", "files"]);
        assert!(servers[0].is_http());
        assert_eq!(servers[1].target(), "npx -y server-filesystem /work");
    }

    #[test]
    fn entries_that_cannot_work_are_left_out_and_named() {
        let (servers, problems) = resolve(&table(
            r#"
[both]
command = "x"
url = "https://example.com"

[neither]
args = ["a"]

["dotted.name"]
command = "x"

[ftp]
url = "ftp://example.com"

[mixed]
command = "x"
headers = { A = "b" }
"#,
        ));
        // `mixed` is usable; its headers are only reported.
        assert_eq!(servers, [stdio("mixed", "x", &[])]);
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("'dotted.name'")));
    }

    #[test]
    fn servers_go_only_where_a_harness_takes_them() {
        let servers = [stdio("files", "npx", &[]), http("docs")];
        let all = McpSupport::via(McpChannel::CommandLine, true);
        assert_eq!(
            for_harness(&servers, all, "Claude"),
            (servers.to_vec(), None)
        );

        let (kept, warning) = for_harness(&servers, McpSupport::NONE, "pi");
        assert!(kept.is_empty());
        let warning = warning.unwrap();
        assert!(
            warning.starts_with("pi takes no MCP servers") && warning.ends_with("files, docs"),
            "{warning}"
        );
        assert_eq!(for_harness(&[], McpSupport::NONE, "pi"), (vec![], None));

        let stdio_only = McpSupport::via(McpChannel::CommandLine, false);
        let (kept, warning) = for_harness(&servers, stdio_only, "X");
        assert_eq!(kept, [servers[0].clone()]);
        assert!(warning.unwrap().ends_with("not passed on: docs"));

        // Over a protocol the session decides about http, later.
        let protocol = McpSupport::via(McpChannel::Protocol, false);
        assert_eq!(for_harness(&servers, protocol, "ACP").0.len(), 2);
    }

    #[test]
    fn probe_does_the_handshake_and_reports_failures() {
        const SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    if m["method"] == "initialize":
        r = {"capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1.2"}}
    else:
        r = {"tools": [{"name": "a"}, {"name": "b"}]}
    print("a log line")
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        let timeout = Duration::from_secs(20);
        let probe = probe_stdio(&stdio("fake", "python3", &["-c", SERVER]), timeout).unwrap();
        assert_eq!(
            probe,
            McpProbe {
                server: "fake 1.2".into(),
                tools: 2
            }
        );

        let gone = probe_stdio(&stdio("gone", "/nonexistent/mcp-server", &[]), timeout);
        assert!(gone.unwrap_err().starts_with("could not start"));
        let quiet = probe_stdio(&stdio("quiet", "true", &[]), timeout);
        assert!(quiet.is_err());
        let slow = probe_stdio(&stdio("slow", "sleep", &["30"]), Duration::from_millis(200));
        assert!(slow.unwrap_err().contains("no answer to initialize"));
    }
}
