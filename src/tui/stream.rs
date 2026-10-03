use anyhow::Result;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;

use crate::harness::HarnessKind;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThoughtDelta(String),
    ToolCall {
        name: String,
        summary: String,
    },
    #[allow(dead_code)]
    Status(String),
    Done,
    Error(String),
}

pub struct StreamRunConfig {
    pub harness: HarnessKind,
    pub binary: PathBuf,
    pub prompt: String,
    pub is_continuation: bool,
    pub auto_approve: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: PathBuf,
}

pub async fn spawn_stream_task(
    cfg: StreamRunConfig,
    tx: Sender<StreamEvent>,
) -> Result<tokio::process::Child> {
    let mut cmd = Command::new(&cfg.binary);
    cmd.current_dir(&cfg.cwd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    match cfg.harness {
        HarnessKind::Agy => {
            cmd.arg("--output-format").arg("stream-json");
            if cfg.auto_approve {
                cmd.arg("--dangerously-skip-permissions");
            }
            if let Some(ref m) = cfg.model {
                cmd.arg("--model").arg(m);
            }
            if let Some(ref e) = cfg.effort {
                cmd.arg("--effort").arg(e);
            }
            if cfg.is_continuation {
                cmd.arg("--continue");
            }
            cmd.arg("--print").arg(&cfg.prompt);
        }
        HarnessKind::Claude => {
            cmd.arg("-p");
            cmd.arg("--verbose");
            cmd.arg("--output-format").arg("stream-json");
            if cfg.auto_approve {
                cmd.arg("--dangerously-skip-permissions");
            }
            if let Some(ref m) = cfg.model {
                cmd.arg("--model").arg(m);
            }
            if let Some(ref e) = cfg.effort {
                cmd.arg("--effort").arg(e);
            }
            if cfg.is_continuation {
                cmd.arg("--continue");
            }
            cmd.arg(&cfg.prompt);
        }
        HarnessKind::Codex => {
            if cfg.auto_approve {
                cmd.arg("--full-auto");
            }
            if let Some(ref m) = cfg.model {
                cmd.arg("--model").arg(m);
            }
            if let Some(ref e) = cfg.effort {
                cmd.arg("--effort").arg(e);
            }
            cmd.arg("exec").arg(&cfg.prompt);
        }
    }

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("Child stdout piped");
    let stderr = child.stderr.take().expect("Child stderr piped");
    let harness = cfg.harness;

    // Spawn reader for stdout
    let tx_out = tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match harness {
                HarnessKind::Agy => {
                    parse_agy_line(trimmed, &tx_out).await;
                }
                HarnessKind::Claude => {
                    parse_claude_line(trimmed, &tx_out).await;
                }
                HarnessKind::Codex => {
                    let _ = tx_out.send(StreamEvent::TextDelta(line + "\n")).await;
                }
            }
        }
        let _ = tx_out.send(StreamEvent::Done).await;
    });

    // Spawn reader for stderr
    let tx_err = tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with("Debugger") {
                let _ = tx_err.send(StreamEvent::Error(line)).await;
            }
        }
    });

    Ok(child)
}

async fn parse_agy_line(line: &str, tx: &Sender<StreamEvent>) {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
        && let Some(event) = val.get("event").and_then(|e| e.as_str())
    {
        match event {
            "step_update" => {
                if let Some(step) = val.get("step_update") {
                    let step_type = step.get("step_type").and_then(|s| s.as_str()).unwrap_or("");
                    let delta = step
                        .get("text_delta")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");

                    match step_type {
                        "agent_response" => {
                            if !delta.is_empty() {
                                let _ = tx.send(StreamEvent::TextDelta(delta.to_string())).await;
                            }
                        }
                        "thought" => {
                            if !delta.is_empty() {
                                let _ = tx.send(StreamEvent::ThoughtDelta(delta.to_string())).await;
                            }
                        }
                        "tool_use" => {
                            let name = step
                                .get("tool_name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let input =
                                step.get("input").map(|i| i.to_string()).unwrap_or_default();
                            let _ = tx
                                .send(StreamEvent::ToolCall {
                                    name,
                                    summary: input,
                                })
                                .await;
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                // Result finalized
            }
            _ => {}
        }
        return;
    }

    // Fallback: non-JSON raw line
    let _ = tx
        .send(StreamEvent::TextDelta(line.to_string() + "\n"))
        .await;
}

async fn parse_claude_line(line: &str, tx: &Sender<StreamEvent>) {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
        let msg_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "assistant" => {
                if let Some(message) = val.get("message")
                    && let Some(content) = message.get("content").and_then(|c| c.as_array())
                {
                    for block in content {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if block_type == "text" {
                            if let Some(txt) = block.get("text").and_then(|t| t.as_str()) {
                                let _ = tx.send(StreamEvent::TextDelta(txt.to_string())).await;
                            }
                        } else if block_type == "thinking" {
                            if let Some(thk) = block.get("thinking").and_then(|t| t.as_str()) {
                                let _ = tx.send(StreamEvent::ThoughtDelta(thk.to_string())).await;
                            }
                        } else if block_type == "tool_use" {
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let input = block
                                .get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_default();
                            let _ = tx
                                .send(StreamEvent::ToolCall {
                                    name,
                                    summary: input,
                                })
                                .await;
                        }
                    }
                }
            }
            "result" => {
                // Done
            }
            _ => {}
        }
        return;
    }

    // Fallback: non-JSON raw line
    let _ = tx
        .send(StreamEvent::TextDelta(line.to_string() + "\n"))
        .await;
}
