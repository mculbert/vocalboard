//! Task queue and sidecar manager: dispatches ML inference requests to the Python process.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context as _, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use proto::{ErrorMsg, FromSidecar, RequestEnvelope, SidecarStatus, ToSidecar};

type PendingMap = Mutex<HashMap<String, oneshot::Sender<Result<Value, ErrorMsg>>>>;

/// Manages the lifecycle of the Python sidecar process and routes IPC messages.
///
/// Spawn via [`SidecarManager::start`]; hold the returned value as Tauri managed
/// state so commands can call [`SidecarManager::ping`].
pub struct SidecarManager {
    stdin: Mutex<ChildStdin>,
    /// In-flight requests awaiting a sidecar response, keyed by `request_id`.
    pending: Arc<PendingMap>,
    status: Arc<RwLock<SidecarStatus>>,
    /// Kept alive so the child process is not dropped while the manager exists.
    _child: Mutex<Child>,
}

impl SidecarManager {
    /// Spawn the Python sidecar and wait up to 30 seconds for the typed `Ready` signal.
    ///
    /// In dev, pass `python_bin = "python"` and `python_args = &["-m", "vocalboard_sidecar"]`.
    pub async fn start(python_bin: &str, python_args: &[&str]) -> Result<Arc<Self>> {
        let mut child = Command::new(python_bin)
            .args(python_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn sidecar: {python_bin}"))?;

        let stdin = child.stdin.take().context("child stdin not captured")?;
        let stdout = child.stdout.take().context("child stdout not captured")?;
        let stderr = child.stderr.take().context("child stderr not captured")?;

        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let status = Arc::new(RwLock::new(SidecarStatus::NotStarted));

        let manager = Arc::new(Self {
            stdin: Mutex::new(stdin),
            pending: Arc::clone(&pending),
            status: Arc::clone(&status),
            _child: Mutex::new(child),
        });

        // Pipe sidecar stderr (structlog output) to tracing at debug level.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "sidecar::stderr", "{line}");
            }
        });

        // Channel that fires once the typed Ready signal is seen on stdout.
        let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

        let pending2 = Arc::clone(&pending);
        let status2 = Arc::clone(&status);
        let ready_tx2 = Arc::clone(&ready_tx);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        Self::route_line(&line, &pending2, &status2, &ready_tx2).await;
                    }
                    Ok(None) => {
                        info!("sidecar stdout closed");
                        break;
                    }
                    Err(e) => {
                        error!("sidecar stdout read error: {e}");
                        break;
                    }
                }
            }
            // Unblock caller if stdout closes before the ready signal.
            let mut guard = ready_tx2.lock().await;
            if let Some(tx) = guard.take() {
                let _ = tx.send(Err(anyhow::anyhow!("sidecar stdout closed before ready")));
            }
            *status2.write().await = SidecarStatus::Error;
        });

        match timeout(Duration::from_secs(30), ready_rx).await {
            Ok(Ok(Ok(()))) => {
                info!("sidecar ready");
                Ok(manager)
            }
            Ok(Ok(Err(e))) => bail!("sidecar failed before ready: {e}"),
            Ok(Err(_)) => bail!("ready channel dropped before signal"),
            Err(_) => bail!("sidecar did not become ready within 30 s"),
        }
    }

    /// Send a `ping` request to the sidecar and return the pong result.
    pub async fn ping(&self) -> Result<proto::PingResult> {
        let payload = self.send("ping", 1, serde_json::json!({})).await?;
        serde_json::from_value(payload).context("invalid ping response payload")
    }

    /// Returns the current sidecar lifecycle status.
    pub async fn status(&self) -> SidecarStatus {
        self.status.read().await.clone()
    }

    // ── private ───────────────────────────────────────────────────────────────

    /// Serialize and send a request; await the result payload with a 30 s timeout.
    async fn send(&self, command: &str, version: u32, payload: Value) -> Result<Value> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();

        let msg = ToSidecar::Request(RequestEnvelope {
            request_id: request_id.clone(),
            command: command.to_string(),
            version,
            payload,
        });
        let line = serde_json::to_string(&msg).context("serialize request")?;

        // Register the waiter before writing: the sidecar can route a response
        // before this task is rescheduled, so the entry must already exist or the
        // reply is dropped as "unknown request_id" and the request times out. On a
        // write/flush failure we remove the entry so it does not leak.
        self.pending.lock().await.insert(request_id.clone(), tx);
        let write = async {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .context("write to sidecar stdin")?;
            stdin.flush().await.context("flush sidecar stdin")?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = write {
            self.pending.lock().await.remove(&request_id);
            return Err(e);
        }

        match timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(val))) => Ok(val),
            Ok(Ok(Err(e))) => bail!("sidecar error {:?}: {}", e.code, e.message),
            Ok(Err(_)) => bail!("response channel dropped for {request_id}"),
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                bail!("sidecar request timed out: {request_id}")
            }
        }
    }

    /// Parse one NDJSON stdout line and route it to the appropriate waiter or logger.
    async fn route_line(
        line: &str,
        pending: &PendingMap,
        status: &RwLock<SidecarStatus>,
        ready_tx: &Mutex<Option<oneshot::Sender<Result<()>>>>,
    ) {
        let msg: FromSidecar = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                warn!("sidecar: unrecognised stdout line ({e}): {line}");
                return;
            }
        };

        match msg {
            FromSidecar::Ready => {
                *status.write().await = SidecarStatus::Ready;
                let mut guard = ready_tx.lock().await;
                if let Some(tx) = guard.take() {
                    let _ = tx.send(Ok(()));
                }
            }
            FromSidecar::Log(log) => {
                info!(target: "sidecar::log", level = ?log.level, "{}", log.msg);
            }
            FromSidecar::Progress(p) => {
                debug!(
                    target: "sidecar::progress",
                    request_id = %p.request_id,
                    step = %p.step,
                    pct = p.pct,
                );
            }
            FromSidecar::Result(r) => {
                let mut map = pending.lock().await;
                if let Some(tx) = map.remove(&r.request_id) {
                    let _ = tx.send(Ok(r.payload));
                } else {
                    warn!("sidecar: result for unknown request_id {}", r.request_id);
                }
            }
            FromSidecar::Error(e) => {
                // Extract request_id before potentially moving e into the channel.
                let rid = e.request_id.as_deref().unwrap_or("").to_owned();
                let mut map = pending.lock().await;
                if let Some(tx) = map.remove(&rid) {
                    let _ = tx.send(Err(e));
                } else {
                    error!(target: "sidecar::error", code = ?e.code, "{}", e.message);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingMap, SidecarManager};
    use std::sync::Arc;
    use tokio::sync::{oneshot, Mutex, RwLock};

    fn python_bin() -> String {
        std::env::var("VOCALBOARD_PYTHON").unwrap_or_else(|_| "python".to_owned())
    }

    /// `route_line` must fire the ready oneshot and set status to Ready when it
    /// receives `{"type":"ready"}`.
    #[tokio::test]
    async fn route_line_fires_ready_on_ready_signal() -> anyhow::Result<()> {
        use std::collections::HashMap;

        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let status = Arc::new(RwLock::new(proto::SidecarStatus::NotStarted));
        let (ready_tx, mut ready_rx) = oneshot::channel::<anyhow::Result<()>>();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

        SidecarManager::route_line(r#"{"type":"ready"}"#, &pending, &status, &ready_tx).await;

        let result = ready_rx.try_recv()?;
        assert!(result.is_ok(), "ready oneshot should carry Ok(())");
        assert!(
            matches!(*status.read().await, proto::SidecarStatus::Ready),
            "status should be Ready after the typed ready signal"
        );
        Ok(())
    }

    /// `route_line` must resolve a pending request as `Err` when the sidecar emits
    /// `unknown_command`, rather than leaving the waiter to time out.
    #[tokio::test]
    async fn route_line_resolves_unknown_command_as_err() -> anyhow::Result<()> {
        use anyhow::Context as _;
        use std::collections::HashMap;

        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let status = Arc::new(RwLock::new(proto::SidecarStatus::NotStarted));
        let (ready_tx, _ready_rx) = oneshot::channel::<anyhow::Result<()>>();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

        let (tx, mut rx) = oneshot::channel();
        pending.lock().await.insert("rid-1".to_string(), tx);

        let line = r#"{"type":"error","request_id":"rid-1","code":"unknown_command","message":"no such command"}"#;
        SidecarManager::route_line(line, &pending, &status, &ready_tx).await;

        let result = rx
            .try_recv()
            .context("route_line should have resolved the pending sender")?;
        match result {
            Err(err) => {
                assert!(
                    matches!(err.code, proto::ErrorCode::UnknownCommand),
                    "expected UnknownCommand, got {:?}",
                    err.code
                );
            }
            Ok(_) => anyhow::bail!("unknown_command should resolve as Err, not Ok"),
        }
        Ok(())
    }

    /// A failed `send` (stdin already closed) must not leave an orphaned entry in
    /// the pending map; the waiter would otherwise never resolve.
    ///
    /// Set `SKIP_SIDECAR_TESTS=1` to skip when Python is unavailable.
    #[tokio::test]
    async fn send_removes_pending_entry_on_write_failure() -> anyhow::Result<()> {
        use anyhow::Context as _;
        use std::collections::HashMap;
        use tokio::process::Command;

        if std::env::var("SKIP_SIDECAR_TESTS").as_deref() == Ok("1") {
            eprintln!("SKIP_SIDECAR_TESTS=1; skipping");
            return Ok(());
        }

        // Spawn a process that exits immediately so the read end of its stdin pipe
        // closes; subsequent writes to the captured stdin fail with BrokenPipe.
        let mut child = Command::new(python_bin())
            .args(["-c", "pass"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("spawn dummy child")?;
        let stdin = child.stdin.take().context("child stdin not captured")?;
        child.wait().await.context("await dummy child exit")?;

        let mgr = SidecarManager {
            stdin: Mutex::new(stdin),
            pending: Arc::new(Mutex::new(HashMap::new())),
            status: Arc::new(RwLock::new(proto::SidecarStatus::NotStarted)),
            _child: Mutex::new(child),
        };

        let result = mgr.send("ping", 1, serde_json::json!({})).await;
        assert!(result.is_err(), "send should fail when stdin is closed");
        assert!(
            mgr.pending.lock().await.is_empty(),
            "pending map must not leak an entry on write failure"
        );
        Ok(())
    }

    /// Spawn the sidecar, await the ready handshake, then ping and check pong.
    ///
    /// Set `SKIP_SIDECAR_TESTS=1` to skip when Python is unavailable.
    #[tokio::test]
    async fn sidecar_start_and_ping() -> anyhow::Result<()> {
        if std::env::var("SKIP_SIDECAR_TESTS").as_deref() == Ok("1") {
            eprintln!("SKIP_SIDECAR_TESTS=1; skipping");
            return Ok(());
        }
        let bin = python_bin();
        let mgr = SidecarManager::start(&bin, &["-m", "vocalboard_sidecar"]).await?;
        let result = mgr.ping().await?;
        assert!(result.pong, "expected pong == true");
        Ok(())
    }
}
