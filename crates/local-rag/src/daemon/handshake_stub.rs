//! The store socket's minimal, explicitly provisional greeting responder
//! (spec 02 §4.1 step 4) — T15-01.
//!
//! See [`super::probe`]'s own module doc for why this exists (the store-lock
//! liveness probe needs *something* live to talk to before T15-02's real
//! handshake protocol exists) and what its relationship to T15-02 is: that
//! task replaces only this per-connection handler with a real framed
//! HELLO/WELCOME parse, never the listener, the bind, or the store lock this
//! task ships.

use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{oneshot, watch};

use super::mode::DaemonMode;
use super::probe::Greeting;

/// Accept connections on `listener`, replying to each with one
/// newline-terminated [`Greeting`] JSON line and closing — the socket's
/// *entire* behavior for T15-01. Runs until `stop` fires, then returns.
///
/// A malformed accept (a transient OS error) is logged nowhere yet (no
/// logging infrastructure exists before this task) and simply retried — the
/// listener itself stays bound; only one accept attempt failed.
pub async fn serve_handshake_stub(
    listener: UnixListener,
    instance_uuid: Arc<str>,
    daemon_version: Arc<str>,
    mode: watch::Receiver<DaemonMode>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stop => return,
            accepted = listener.accept() => {
                let Ok((mut stream, _addr)) = accepted else { continue };
                let greeting = Greeting {
                    instance_uuid: instance_uuid.to_string(),
                    daemon_version: daemon_version.to_string(),
                    mode: mode.borrow().as_str().to_string(),
                };
                if let Ok(mut line) = serde_json::to_vec(&greeting) {
                    line.push(b'\n');
                    let _ = stream.write_all(&line).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::TempHome;
    use std::io::{BufRead, BufReader};

    #[tokio::test]
    async fn replies_with_the_current_mode_and_stops_on_signal() {
        let home = TempHome::new().expect("temp home");
        let socket_path = home.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mode_tx, mode_rx) = watch::channel(DaemonMode::Normal);
        let (stop_tx, stop_rx) = oneshot::channel();

        let handle = tokio::spawn(serve_handshake_stub(
            listener,
            Arc::from("instance-a"),
            Arc::from("0.0.0"),
            mode_rx,
            stop_rx,
        ));

        // Blocking connect on a spawn_blocking thread: this test's own
        // in-process listener is real, but reading it synchronously keeps
        // the test simple without a second async runtime task.
        let path_for_read = socket_path.clone();
        let greeting: Greeting = tokio::task::spawn_blocking(move || {
            let stream = std::os::unix::net::UnixStream::connect(&path_for_read).expect("connect");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("read greeting line");
            serde_json::from_str(line.trim_end()).expect("parse greeting")
        })
        .await
        .expect("blocking task");

        assert_eq!(greeting.instance_uuid, "instance-a");
        assert_eq!(greeting.daemon_version, "0.0.0");
        assert_eq!(greeting.mode, "normal");

        // A mode change takes effect on the *next* connection.
        mode_tx
            .send(DaemonMode::MigrationOnly {
                reason: super::super::mode::MigrationOnlyReason::Other {
                    detail: "test".to_string(),
                },
            })
            .expect("send mode update");
        let path_for_read = socket_path.clone();
        let greeting2: Greeting = tokio::task::spawn_blocking(move || {
            let stream = std::os::unix::net::UnixStream::connect(&path_for_read).expect("connect");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("read greeting line");
            serde_json::from_str(line.trim_end()).expect("parse greeting")
        })
        .await
        .expect("blocking task");
        assert_eq!(greeting2.mode, "migration_only");

        stop_tx.send(()).expect("send stop");
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the accept loop must actually stop on signal")
            .expect("task did not panic");
    }
}
