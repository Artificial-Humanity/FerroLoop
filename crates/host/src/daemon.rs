//! Accept loop on the listener `Host::open` already bound.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use fl_host_proto::{
    ControllerLease, HostConn, HostError, HostRequest, HostResponse, HostedChild, RequestId,
    SendBlock, SendOutcome, SessionId,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::input::WriterHandle;
use crate::output::OutputSession;
use crate::session::{Session, StdoutSink};
use crate::{Host, HostInner};

pub(crate) fn attach(host: &Arc<HostInner>, session: &Arc<Session>) {
    let stdin = session.take_stdin();
    let output = OutputSession::open(Arc::clone(session), Arc::clone(&host.hub));
    let writer = WriterHandle::start(Arc::clone(session), stdin);
    let fence = Arc::clone(&writer);
    output.set_fence(Arc::new(move || fence.fence()));
    let sink: Arc<dyn StdoutSink> = output.clone();
    session.set_sink(Arc::downgrade(&sink));
    host.outputs
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(session.session.as_str().to_owned(), Arc::clone(&output));
    let stop = Arc::clone(host);
    let running = Arc::clone(&writer);
    host.spawn_thread(move || running.run(move || stop.stop.load(Ordering::SeqCst)));
    let previous = host
        .writers
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(session.session.as_str().to_owned(), writer);
    if let Some(previous) = previous {
        previous.shutdown();
    }
    output.emit_spawned(HostedChild {
        state: session.state(),
    });
}

pub(crate) async fn accept_loop(host: Arc<HostInner>) {
    let mut listener = {
        let mut slot = host.listener.lock().unwrap_or_else(|err| err.into_inner());
        match slot.take() {
            Some(listener) => listener,
            None => return,
        }
    };
    loop {
        if host.stop.load(Ordering::SeqCst) {
            *host.listener.lock().unwrap_or_else(|err| err.into_inner()) = Some(listener);
            return;
        }
        tokio::select! {
            biased;
            _ = host.wake.notified() => {
                if host.stop.load(Ordering::SeqCst) {
                    *host.listener.lock().unwrap_or_else(|err| err.into_inner()) = Some(listener);
                    return;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok(conn) => {
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            serve_conn(host, conn).await;
                        });
                    }
                    Err(_) if host.stop.load(Ordering::SeqCst) => return,
                    Err(_) => {}
                }
            }
        }
    }
}

async fn serve_conn(host: Arc<HostInner>, mut conn: HostConn) {
    let conn_id = host.next_conn.fetch_add(1, Ordering::SeqCst);
    // One connection has one outstanding client correlator, but the daemon still
    // reads the next frame while a writer result is pending so the owning
    // connection can admit a bounded queue of sends and authority ops.
    let (tx, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    loop {
        if host.stop.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            biased;
            response = incoming.recv() => {
                let Some(response) = response else {
                    break;
                };
                if conn.respond(&response).await.is_err() {
                    break;
                }
            }
            request = conn.recv() => {
                let request = match request {
                    Ok(request) => request,
                    Err(_) => break,
                };
                if let HostRequest::Events { id, session } = request {
                    match begin_events(Arc::clone(&host), conn, conn_id, id, session).await {
                        AfterEvents::Keep(returned) => conn = returned,
                        AfterEvents::Stream => return,
                        AfterEvents::Closed => break,
                    }
                    continue;
                }
                if let HostRequest::Spawn { id, spec } = request {
                    let host = Arc::clone(&host);
                    let joined =
                        tokio::task::spawn_blocking(move || Host::spawn_session(&host, &spec))
                            .await;
                    let response = match joined {
                        Ok(Ok(handle)) => HostResponse::Spawned {
                            id,
                            child: HostedChild {
                                state: handle.state(),
                            },
                        },
                        Ok(Err(err)) => HostResponse::Failed {
                            id,
                            reason: spawn_reason(&err),
                        },
                        Err(_) => HostResponse::Failed {
                            id,
                            reason: "spawn failed".into(),
                        },
                    };
                    let _ = tx.send(response);
                    continue;
                }
                dispatch(&host, conn_id, request, &tx);
            }
        }
    }
    revoke(&host, conn_id);
}

fn revoke(host: &HostInner, conn_id: u64) {
    let writers: Vec<_> = host
        .writers
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .values()
        .cloned()
        .collect();
    for writer in writers {
        writer.revoke(conn_id);
    }
}

fn writer(host: &HostInner, name: &str) -> Option<Arc<WriterHandle>> {
    host.writers
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(name)
        .cloned()
}

fn spawn_reason(err: &HostError) -> String {
    let text = err.to_string();
    if text.contains("spawn rejected") {
        format!("duplicate session: {text}")
    } else {
        text
    }
}

fn dispatch(
    host: &Arc<HostInner>,
    conn_id: u64,
    request: HostRequest,
    tx: &UnboundedSender<HostResponse>,
) {
    match request {
        HostRequest::Spawn { .. } | HostRequest::Events { .. } => {}
        HostRequest::List { id } => {
            let children = {
                let sessions = host.sessions.lock().unwrap_or_else(|err| err.into_inner());
                sessions
                    .values()
                    .map(|session| HostedChild {
                        state: session.state(),
                    })
                    .collect()
            };
            let _ = tx.send(HostResponse::Listed { id, children });
        }
        HostRequest::Send {
            id,
            session,
            expected_epoch,
            lease,
            line,
        } => {
            let Some(writer) = writer(host, session.as_str()) else {
                let _ = tx.send(HostResponse::Sent {
                    id,
                    outcome: SendOutcome::NotWritten {
                        reason: SendBlock::StaleEpoch,
                    },
                });
                return;
            };
            match writer.send(conn_id, expected_epoch.0, lease, line) {
                Err(outcome) => {
                    let _ = tx.send(HostResponse::Sent { id, outcome });
                }
                Ok(rx) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let outcome = rx.await.unwrap_or(SendOutcome::NotWritten {
                            reason: SendBlock::Io,
                        });
                        let _ = tx.send(HostResponse::Sent { id, outcome });
                    });
                }
            }
        }
        HostRequest::ClaimLease {
            id,
            session,
            expected_epoch,
        } => {
            let Some(writer) = writer(host, session.as_str()) else {
                let _ = tx.send(HostResponse::Failed {
                    id,
                    reason: "unknown session".into(),
                });
                return;
            };
            match writer.claim(conn_id, expected_epoch.0) {
                Err(reason) => {
                    let _ = tx.send(HostResponse::Failed { id, reason });
                }
                Ok(rx) => schedule(tx, rx, id, move |counter| HostResponse::Lease {
                    id,
                    lease: ControllerLease(counter),
                }),
            }
        }
        HostRequest::Signal {
            id,
            session,
            expected_epoch,
            lease,
            signal,
        } => {
            let Some(writer) = writer(host, session.as_str()) else {
                let _ = tx.send(HostResponse::Failed {
                    id,
                    reason: "unknown session".into(),
                });
                return;
            };
            match writer.signal(conn_id, expected_epoch.0, lease, signal) {
                Err(reason) => {
                    let _ = tx.send(HostResponse::Failed { id, reason });
                }
                Ok(rx) => schedule(tx, rx, id, move |_| HostResponse::Signaled { id }),
            }
        }
        HostRequest::Kill {
            id,
            session,
            expected_epoch,
            lease,
        } => {
            let Some(writer) = writer(host, session.as_str()) else {
                let _ = tx.send(HostResponse::Failed {
                    id,
                    reason: "unknown session".into(),
                });
                return;
            };
            match writer.kill(conn_id, expected_epoch.0, lease) {
                Err(reason) => {
                    let _ = tx.send(HostResponse::Failed { id, reason });
                }
                Ok(rx) => schedule(tx, rx, id, move |_| HostResponse::Killed { id }),
            }
        }
    }
}

fn schedule(
    tx: &UnboundedSender<HostResponse>,
    rx: oneshot::Receiver<Result<u64, String>>,
    id: RequestId,
    success: impl FnOnce(u64) -> HostResponse + Send + 'static,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let response = match rx.await {
            Ok(Ok(counter)) => success(counter),
            Ok(Err(reason)) => HostResponse::Failed { id, reason },
            Err(_) => return,
        };
        let _ = tx.send(response);
    });
}

enum AfterEvents {
    Keep(HostConn),
    Stream,
    Closed,
}

async fn begin_events(
    host: Arc<HostInner>,
    mut conn: HostConn,
    conn_id: u64,
    id: RequestId,
    session: Option<SessionId>,
) -> AfterEvents {
    let kill = conn.dup_shutdown();
    match host.hub.subscribe(session, kill) {
        Err((kill, reason)) => {
            if let Some(kill) = kill
                && let Ok(kill) = Arc::try_unwrap(kill)
            {
                kill.disarm();
            }
            if conn
                .respond(&HostResponse::Failed {
                    id,
                    reason: reason.to_owned(),
                })
                .await
                .is_err()
            {
                return AfterEvents::Closed;
            }
            AfterEvents::Keep(conn)
        }
        Ok(sub) => {
            if conn
                .respond(&HostResponse::Subscribed { id })
                .await
                .is_err()
            {
                sub.abandon(&host.hub);
                return AfterEvents::Closed;
            }
            conn.enter_stream();
            conn.shrink_send_buffer();
            let handle = host.handle.clone();
            let hub = Arc::clone(&host.hub);
            let host_for_thread = Arc::clone(&host);
            host.spawn_thread(move || {
                let conn = std::sync::Mutex::new(conn);
                let writing = std::sync::Arc::new(conn);
                let probing = std::sync::Arc::clone(&writing);
                sub.run(
                    hub,
                    move |frame| {
                        let mut conn = writing.lock().unwrap_or_else(|err| err.into_inner());
                        handle.block_on(conn.write_frame(&frame)).map_err(|_| ())
                    },
                    move || {
                        probing
                            .lock()
                            .unwrap_or_else(|err| err.into_inner())
                            .peer_closed()
                    },
                );
                revoke(&host_for_thread, conn_id);
            });
            AfterEvents::Stream
        }
    }
}
