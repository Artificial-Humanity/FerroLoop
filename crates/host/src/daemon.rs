//! Accept loop on the listener `Host::open` already bound.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use fl_host_proto::{
    ControllerLease, HostConn, HostError, HostRequest, HostResponse, HostedChild, RequestId,
    SendBlock, SendOutcome, SessionId,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::authority;
use crate::input::WriterHandle;
use crate::output::OutputSession;
use crate::session::{Session, StdoutSink};
use crate::{Host, HostInner};
use fl_host_proto::{
    ConnId, ExtensionRefusal as Refusal, ExtensionRequest as XReq, ExtensionResponse as XResp,
    SessionMode,
};

pub(crate) fn attach(
    host: &Arc<HostInner>,
    session: &Arc<Session>,
    admission: Option<authority::PreparedEpoch>,
) {
    let stdin = session.take_stdin();
    let output = OutputSession::open(Arc::clone(session), Arc::clone(&host.hub));
    let writer = WriterHandle::start(Arc::clone(session), stdin);
    if let Some(admission) = admission {
        host.authority.register(admission, session, &writer);
    }
    let fence = Arc::downgrade(&writer);
    output.set_fence(Arc::new(move || {
        if let Some(writer) = fence.upgrade() {
            writer.fence();
        }
    }));
    let sink: Arc<dyn StdoutSink> = output.clone();
    session.set_sink(Arc::downgrade(&sink));
    host.outputs
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(
            (session.session.as_str().to_owned(), session.epoch),
            Arc::clone(&output),
        );
    let stop = Arc::clone(host);
    let running = Arc::clone(&writer);
    host.spawn_epoch_thread(session, move || {
        running.run(move || stop.stop.load(Ordering::SeqCst))
    });
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
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {host.authority.expire(std::time::Instant::now());}
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

#[allow(deprecated)]
async fn serve_conn(host: Arc<HostInner>, mut conn: HostConn) {
    let Ok(conn_id) = host
        .next_conn
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
    else {
        return;
    };
    if host.authority.connect(ConnId(conn_id)).is_err() {
        return;
    }
    let mut pending = 0usize;
    let extended_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (extended_tx, mut extended_rx) = tokio::sync::mpsc::unbounded_channel::<ExtendedReply>();
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
            reply = extended_rx.recv() => {
                let Some(reply)=reply else{break;};
                if let Some(dir)=crate::input::hold_dir(){
                    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
                    while matches!(crate::input::poll_barrier(&dir,"reliable-response-write",Some(deadline)),crate::input::Barrier::Waiting) {
                        let _=std::fs::write(dir.join("reliable-pending-count"),extended_count.load(Ordering::SeqCst).to_string());
                        if host.stop.load(Ordering::SeqCst){break;}
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                let failed=write_extended_reply(&mut conn, reply).await.is_err();
                if failed {break;}
            }
            response = incoming.recv() => {
                let Some(response) = response else {
                    break;
                };
                pending=pending.saturating_sub(1);
                if conn.respond(&response).await.is_err() {
                    break;
                }
            }
            request = conn.recv() => {
                let request = match request {
                    Ok(request) => request,
                    Err(_) => break,
                };
                if let HostRequest::Extended {id,request:XReq::OpenStream{ticket}}=request {
                    if pending!=0 || extended_count.load(Ordering::SeqCst)!=0 {
                        if conn.respond(&HostResponse::Extended{id,response:XResp::Refused{code:Refusal::Busy}}).await.is_err(){break;}
                    } else {
                        match begin_extension(Arc::clone(&host),conn,conn_id,id,ticket).await {
                            AfterEvents::Keep(returned)=>conn=returned,
                            AfterEvents::Stream=>return,
                            AfterEvents::Closed=>break,
                        }
                    }
                    continue;
                }
                if let HostRequest::Events { id, session } = request {
                    if pending != 0 || extended_count.load(Ordering::SeqCst) != 0 {
                        if conn.respond(&HostResponse::Failed { id, reason: "busy".into() }).await.is_err() { break; }
                        continue;
                    }
                    match begin_events(Arc::clone(&host), conn, conn_id, id, session).await {
                        AfterEvents::Keep(returned) => conn = returned,
                        AfterEvents::Stream => return,
                        AfterEvents::Closed => break,
                    }
                    continue;
                }
                if let HostRequest::Extended{id,request}=request {
                    let permit=admit_extension(&host,&extended_count,&request);
                    match permit {
                        Err(code)=>{
                            if conn.respond(&HostResponse::Extended{id,response:XResp::Refused{code}}).await.is_err(){break;}
                        }
                        Ok(permit)=>{
                            let host=Arc::clone(&host);let tx=extended_tx.clone();
                            let dispatch_host=Arc::clone(&host);
                            let result=spawn_extension_operation(&dispatch_host,request,|request|async move {
                                let mut snapshot=None;
                                let response=extended(&host,ConnId(conn_id),request,&mut snapshot).await.unwrap_or_else(|code|XResp::Refused{code});
                                if let Some(dir)=crate::input::hold_dir(){
                                    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
                                    while matches!(crate::input::poll_barrier(&dir,"reliable-operation",Some(deadline)),crate::input::Barrier::Waiting) {
                                        if !host.authority.connection_live(ConnId(conn_id)){return;}
                                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                                    }
                                }
                                let _=tx.send(ExtendedReply{response:HostResponse::Extended{id,response},_permit:permit,_snapshot:snapshot});
                            });
                            match result {
                                Ok(())=>{}
                                Err(code)=>{
                                    if conn.respond(&HostResponse::Extended{id,response:XResp::Refused{code}}).await.is_err(){break;}
                                }
                            }
                        }
                    }
                    continue;
                }
                pending+=1;
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
                dispatch(&host, conn_id, request, &tx).await;
            }
        }
    }
    revoke(&host, conn_id);
}

fn revoke(host: &HostInner, conn_id: u64) {
    authority::revoke_connection(&host.authority, ConnId(conn_id));
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

async fn writer(host: &HostInner, name: &str) -> Option<Arc<WriterHandle>> {
    if let Some(dir) = crate::input::hold_dir() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while matches!(
            crate::input::poll_barrier(&dir, "legacy-writer-lookup", Some(deadline)),
            crate::input::Barrier::Waiting
        ) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    host.writers
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(name)
        .cloned()
        .filter(|writer| writer.mode() == SessionMode::LegacyPipe)
}

fn spawn_reason(err: &HostError) -> String {
    let text = err.to_string();
    if text.contains("spawn rejected") {
        format!("duplicate session: {text}")
    } else {
        text
    }
}

async fn dispatch(
    host: &Arc<HostInner>,
    conn_id: u64,
    request: HostRequest,
    tx: &UnboundedSender<HostResponse>,
) {
    match request {
        HostRequest::Extended { .. } => unreachable!("extension admission is separate"),
        HostRequest::Spawn { .. } | HostRequest::Events { .. } => {}
        HostRequest::List { id } => {
            let children = {
                let sessions = host.sessions.lock().unwrap_or_else(|err| err.into_inner());
                sessions
                    .values()
                    .filter(|session| session.mode() == SessionMode::LegacyPipe)
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
            let Some(writer) = writer(host, session.as_str()).await else {
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
            let Some(writer) = writer(host, session.as_str()).await else {
                let _ = tx.send(HostResponse::Failed {
                    id,
                    reason: "unknown session".into(),
                });
                return;
            };
            match writer.claim(conn_id, expected_epoch.0) {
                Err(reason) => {
                    let _ = tx.send(HostResponse::Failed {
                        id,
                        reason: reason.to_string(),
                    });
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
            let Some(writer) = writer(host, session.as_str()).await else {
                let _ = tx.send(HostResponse::Failed {
                    id,
                    reason: "unknown session".into(),
                });
                return;
            };
            match writer.signal(conn_id, expected_epoch.0, lease, signal) {
                Err(reason) => {
                    let _ = tx.send(HostResponse::Failed {
                        id,
                        reason: reason.to_string(),
                    });
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
            let Some(writer) = writer(host, session.as_str()).await else {
                let _ = tx.send(HostResponse::Failed {
                    id,
                    reason: "unknown session".into(),
                });
                return;
            };
            match writer.kill(conn_id, expected_epoch.0, lease) {
                Err(reason) => {
                    let _ = tx.send(HostResponse::Failed {
                        id,
                        reason: reason.to_string(),
                    });
                }
                Ok(rx) => schedule(tx, rx, id, move |_| HostResponse::Killed { id }),
            }
        }
    }
}

fn schedule(
    tx: &UnboundedSender<HostResponse>,
    rx: oneshot::Receiver<Result<u64, crate::input::ControlError>>,
    id: RequestId,
    success: impl FnOnce(u64) -> HostResponse + Send + 'static,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let response = match rx.await {
            Ok(Ok(counter)) => success(counter),
            Ok(Err(reason)) => HostResponse::Failed {
                id,
                reason: reason.to_string(),
            },
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
    if session.as_ref().is_some_and(|name| {
        host.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name.as_str())
            .is_some_and(|s| s.mode() != SessionMode::LegacyPipe)
    }) {
        return if conn
            .respond(&HostResponse::Failed {
                id,
                reason: "wrong mode".into(),
            })
            .await
            .is_ok()
        {
            AfterEvents::Keep(conn)
        } else {
            AfterEvents::Closed
        };
    }
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
            // Authentication survives the stream transition; RPC authority does not.
            // Revoke before publishing Subscribed so no ticket can race the reply.
            revoke(&host, conn_id);
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
            let minor = conn.negotiated_minor();
            host.spawn_thread(move || {
                let conn = std::sync::Mutex::new(conn);
                let writing = std::sync::Arc::new(conn);
                let probing = std::sync::Arc::clone(&writing);
                sub.run(
                    hub,
                    minor,
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

pub(super) async fn extended(
    host: &Arc<HostInner>,
    conn: ConnId,
    request: XReq,
    snapshot_reservation: &mut Option<crate::slice_budget::Reservation>,
) -> Result<XResp, Refusal> {
    match request {
        XReq::Capabilities => Ok(XResp::Capabilities {
            host: host.authority.instance().clone(),
            #[cfg(unix)]
            features: fl_host_proto::RELIABLE_FEATURE
                | fl_host_proto::PTY_FEATURE
                | fl_host_proto::RENDERED_PTY_FEATURE
                | fl_host_proto::HOST_STATUS_FEATURE
                | fl_host_proto::SESSION_OWNER_FEATURE
                | fl_host_proto::PTY_RETENTION_FEATURE,
            #[cfg(not(unix))]
            features: fl_host_proto::RELIABLE_FEATURE
                | fl_host_proto::HOST_STATUS_FEATURE
                | fl_host_proto::SESSION_OWNER_FEATURE
                | fl_host_proto::PTY_RETENTION_FEATURE,
            limits: fl_host_proto::ExtensionLimits::default(),
        }),
        XReq::HostStatus => {
            host.maintain_completed();
            Ok(XResp::HostStatus {
                build: host.build_info.clone(),
            })
        }
        XReq::PtyRetentionStatus => Ok(XResp::PtyRetentionStatus {
            status: host.pty_retention_status(),
        }),
        XReq::CheckSessionOwner { stream, hosted_id } => {
            let sessions = host.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let matches = stream.host == *host.authority.instance()
                && sessions
                    .get(stream.session.as_str())
                    .is_some_and(|session| {
                        session.epoch == stream.epoch.0
                            && matches!(session.state(), fl_host_proto::ChildState::Running(_))
                            && session.owned_by(&hosted_id)
                    });
            Ok(XResp::SessionOwner { matches })
        }
        XReq::ListSessions => {
            host.maintain_completed();
            let outputs = host.outputs.lock().unwrap_or_else(|e| e.into_inner());
            if outputs.len() > fl_host_proto::DEFAULT_MAX_SESSIONS {
                return Err(Refusal::ResourceLimit);
            }
            // The permit precharges all 64 slots. Never use amortized Vec growth.
            let mut records = Vec::with_capacity(fl_host_proto::DEFAULT_MAX_SESSIONS);
            for output in outputs.values() {
                let session = output.session();
                let (acked, latest) = session.reliable().map(|j| j.position()).unwrap_or((0, 0));
                let state = session.state();
                let input = match &state {
                    fl_host_proto::ChildState::Running(r)
                    | fl_host_proto::ChildState::LeaderExited(r)
                    | fl_host_proto::ChildState::CleanupComplete(r) => r.input.clone(),
                };
                records.push(fl_host_proto::SessionSummary {
                    stream: fl_host_proto::StreamId {
                        host: host.authority.instance().clone(),
                        session: session.session.clone(),
                        epoch: fl_host_proto::SessionEpoch(session.epoch),
                    },
                    mode: session.mode(),
                    state,
                    input,
                    acked,
                    latest,
                });
            }
            Ok(XResp::Sessions { records })
        }
        XReq::SpawnSession { spec, mode, pty } => {
            #[cfg(not(unix))]
            if mode == SessionMode::Pty || spec.kind == fl_host_proto::SpawnKind::Pty {
                return Err(Refusal::Unsupported);
            }
            match (mode, spec.kind, pty) {
                (
                    SessionMode::LegacyPipe | SessionMode::ReliablePipe,
                    fl_host_proto::SpawnKind::Pipe,
                    None,
                ) => {}
                (SessionMode::Pty, fl_host_proto::SpawnKind::Pty, Some(opts))
                    if opts.cols == 0 || opts.rows == 0 =>
                {
                    return Err(Refusal::WrongMode);
                }
                (SessionMode::Pty, fl_host_proto::SpawnKind::Pty, _) => {}
                _ => return Err(Refusal::WrongMode),
            }
            let host2 = Arc::clone(host);
            let child = tokio::task::spawn_blocking(move || {
                Host::spawn_session_mode(&host2, &spec, mode, pty)
            })
            .await
            .map_err(|_| Refusal::Io)?
            .map_err(|_| Refusal::ResourceLimit)?;
            let state = child.state();
            let record = match &state {
                fl_host_proto::ChildState::Running(r)
                | fl_host_proto::ChildState::LeaderExited(r)
                | fl_host_proto::ChildState::CleanupComplete(r) => r,
            };
            Ok(XResp::Session {
                stream: fl_host_proto::StreamId {
                    host: host.authority.instance().clone(),
                    session: record.session.clone(),
                    epoch: record.session_epoch,
                },
                state,
            })
        }
        XReq::ClaimController { stream } => {
            let grant = authority::claim(&host.authority, conn, &stream).await?;
            Ok(XResp::Controller {
                lease: grant.lease,
                acked: grant.acked,
                latest: grant.latest,
            })
        }
        XReq::SubscribeReliable {
            stream,
            lease,
            after_seq,
        } => {
            let cursor = host
                .authority
                .subscription_cursor(conn, stream, lease, after_seq)?;
            Ok(authority::issue_ticket(
                &host.authority,
                conn,
                cursor,
                Some(lease),
                false,
                std::time::Instant::now(),
            )?
            .response())
        }
        XReq::Observe { stream, after_seq } => {
            // Observer progress never grants a durable recovery baseline.
            let after = after_seq.unwrap_or(0);
            Ok(authority::issue_ticket(
                &host.authority,
                conn,
                fl_host_proto::Cursor { stream, seq: after },
                None,
                true,
                std::time::Instant::now(),
            )?
            .response())
        }
        XReq::Ack { cursor, lease } => {
            let through = cursor.seq;
            host.authority.acknowledge(conn, cursor, lease).await?;
            if let Some(dir) = crate::input::hold_dir() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while matches!(
                    crate::input::poll_barrier(&dir, "reliable-ack-reply", Some(deadline)),
                    crate::input::Barrier::Waiting
                ) {
                    if !host.authority.connection_live(conn) || host.stop.load(Ordering::SeqCst) {
                        return Err(Refusal::WrongConnection);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
            Ok(XResp::Acked { through })
        }
        XReq::SendLine {
            stream,
            lease,
            line,
        } => {
            let (writer, writer_lease) = host.authority.mutation(conn, &stream, lease)?;
            let outcome = match writer.send_reliable(
                conn.0,
                stream.epoch.0,
                writer_lease,
                line,
                &host.reliable_budget,
            )? {
                Ok(rx) => rx.await.map_err(|_| Refusal::Io)?,
                Err(outcome) => outcome,
            };
            Ok(XResp::Input { outcome })
        }
        XReq::SignalSession {
            stream,
            lease,
            signal,
        } => {
            let (writer, writer_lease) = host.authority.mutation(conn, &stream, lease)?;
            writer
                .signal(conn.0, stream.epoch.0, writer_lease, signal)
                .map_err(|_| Refusal::SessionFenced)?
                .await
                .map_err(|_| Refusal::Io)?
                .map_err(|_| Refusal::Io)?;
            Ok(XResp::Controlled)
        }
        XReq::KillSession { stream, lease } => {
            let (writer, writer_lease) = host.authority.mutation(conn, &stream, lease)?;
            writer
                .kill(conn.0, stream.epoch.0, Some(writer_lease))
                .map_err(|_| Refusal::SessionFenced)?
                .await
                .map_err(|_| Refusal::Io)?
                .map_err(|_| Refusal::Io)?;
            Ok(XResp::Controlled)
        }
        XReq::Write {
            stream,
            lease,
            bytes,
        } => {
            let (writer, writer_lease) = host.authority.mutation(conn, &stream, lease)?;
            let outcome = match writer.write_raw_reliable(
                conn.0,
                stream.epoch.0,
                writer_lease,
                bytes.into_vec(),
                &host.reliable_budget,
            )? {
                Ok(rx) => rx.await.map_err(|_| Refusal::Io)?,
                Err(outcome) => outcome,
            };
            Ok(XResp::Input { outcome })
        }
        XReq::CaptureRendered { stream } => {
            if !cfg!(unix) {
                return Err(Refusal::Unsupported);
            }
            let session = host.authority.rendered_session(conn, &stream)?;
            let snapshot = session.capture_screen()?;
            let screen = fl_host_proto::RenderedScreen {
                schema: 1,
                stream,
                contents: snapshot.contents,
                cursor_row: snapshot.cursor_row,
                cursor_col: snapshot.cursor_col,
                rows: snapshot.rows,
                cols: snapshot.cols,
                generation: snapshot.generation,
                output_seq: snapshot.output_seq,
                output_bytes: snapshot.output_bytes,
            };
            screen.validate().map_err(|_| Refusal::ResourceLimit)?;
            *snapshot_reservation = Some(snapshot._reservation);
            Ok(XResp::RenderedCapture { screen })
        }
        XReq::Resize {
            stream,
            lease,
            cols,
            rows,
        } => {
            if !cfg!(unix) {
                return Err(Refusal::Unsupported);
            }
            fl_host_proto::validate_pty_geometry(cols, rows).map_err(|_| Refusal::ResourceLimit)?;
            let (writer, writer_lease) = host.authority.mutation(conn, &stream, lease)?;
            let revision = writer
                .resize(
                    conn,
                    stream,
                    (lease, writer_lease),
                    (cols, rows),
                    &host.authority,
                    &host.reliable_budget,
                )?
                .await
                .map_err(|_| Refusal::SessionFenced)?
                .map_err(crate::input::ControlError::into_refusal)?;
            Ok(XResp::Resized {
                cols,
                rows,
                projection_revision: revision,
            })
        }
        XReq::CaptureOutput { session, max_bytes } => {
            let found = host
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(session.as_str())
                .cloned();
            let Some(session) = found else {
                return Err(Refusal::StaleEpoch);
            };
            let max_bytes = (max_bytes as usize).min(fl_host_proto::MAX_RAW_INPUT);
            Ok(XResp::CapturedOutput {
                text: session.stdout_tail(max_bytes),
            })
        }
        _ => Err(Refusal::Unsupported),
    }
}
async fn begin_extension(
    host: Arc<HostInner>,
    mut conn: HostConn,
    conn_id: u64,
    id: RequestId,
    ticket: fl_host_proto::Ticket,
) -> AfterEvents {
    // The event connection's authenticated agreement is authoritative independently.
    if conn.negotiated_minor() < 1 {
        return AfterEvents::Closed;
    }
    let active = match authority::consume_ticket(
        &host.authority,
        ConnId(conn_id),
        ticket,
        std::time::Instant::now(),
    ) {
        Ok(active) => active,
        Err(code) => {
            return if conn
                .respond(&HostResponse::Extended {
                    id,
                    response: XResp::Refused { code },
                })
                .await
                .is_ok()
            {
                AfterEvents::Keep(conn)
            } else {
                AfterEvents::Closed
            };
        }
    };
    // This socket is now event-only. Its separate owning RPC remains live.
    revoke(&host, conn_id);
    if conn
        .respond(&HostResponse::Extended {
            id,
            response: XResp::Opened,
        })
        .await
        .is_err()
        || conn.enter_extension_stream().is_err()
    {
        drop(active);
        return AfterEvents::Closed;
    }
    conn.shrink_send_buffer();
    tokio::spawn(async move {
        run_extension(&host, &mut conn, active).await;
        revoke(&host, conn_id);
    });
    AfterEvents::Stream
}
async fn reliable_hold(
    host: &HostInner,
    conn: &HostConn,
    state: &authority::StreamState,
    phase: &str,
    cancellable: bool,
) -> bool {
    reliable_hold_until(
        host,
        conn,
        state,
        phase,
        cancellable,
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    )
    .await
}
async fn reliable_hold_until(
    host: &HostInner,
    conn: &HostConn,
    state: &authority::StreamState,
    phase: &str,
    cancellable: bool,
    deadline: std::time::Instant,
) -> bool {
    let Some(dir) = crate::input::hold_dir() else {
        return true;
    };
    loop {
        if host.stop.load(Ordering::SeqCst)
            || (cancellable && (state.cancelled() || conn.peer_closed()))
        {
            return false;
        }
        match crate::input::poll_barrier(&dir, phase, Some(deadline)) {
            crate::input::Barrier::Cleared | crate::input::Barrier::Pulsed => return true,
            crate::input::Barrier::Expired => return false,
            crate::input::Barrier::Waiting => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await
            }
        }
    }
}
async fn run_extension(
    host: &HostInner,
    conn: &mut HostConn,
    mut active: crate::reliable::ReplayCursor,
) {
    let journal = active.journal();
    let stream_state = Arc::clone(&active.authority.as_ref().expect("consumed ticket").state);
    let rpc = active.authority.as_ref().expect("consumed ticket").rpc();
    let mut parked = None;
    loop {
        if host.stop.load(Ordering::SeqCst)
            || conn.peer_closed()
            || !host.authority.connection_live(rpc)
        {
            break;
        }
        if stream_state.cancelled() {
            if journal.compacted()
                && let Ok(_wire) = crate::slice_budget::reserve(
                    journal.budget(),
                    crate::slice_budget::ChargeKind::Viewer,
                    256 * 1024,
                )
                && let Ok(frame) = fl_host_proto::encode_extension_frame(
                    conn.negotiated_minor(),
                    &fl_host_proto::ExtensionFrame::Notice {
                        code: fl_host_proto::ExtensionRefusal::SessionFenced,
                    },
                )
            {
                let _ = conn.write_frame(&frame).await;
            }
            break;
        }
        if let Some(gap) = active.gap() {
            let Ok(_wire) = crate::slice_budget::reserve(
                journal.budget(),
                crate::slice_budget::ChargeKind::Viewer,
                256 * 1024,
            ) else {
                break;
            };
            let notice = if conn.negotiated_minor() < 2 {
                fl_host_proto::ExtensionFrame::Notice {
                    code: fl_host_proto::ExtensionRefusal::Unsupported,
                }
            } else {
                fl_host_proto::ExtensionFrame::Gap {
                    from_seq: gap.from_seq,
                    next_seq: gap.next_seq,
                }
            };
            let Ok(frame) = fl_host_proto::encode_extension_frame(conn.negotiated_minor(), &notice)
            else {
                break;
            };
            if conn.write_frame(&frame).await.is_err() || conn.negotiated_minor() < 2 {
                break;
            }
            if active.advance_gap(gap).is_err() {
                break;
            }
            continue;
        }
        let record = match active.next() {
            Ok(Some(record)) => record,
            Ok(None) => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
            Err(fl_host_proto::ExtensionRefusal::CursorExpired) if active.gap().is_some() => {
                continue;
            }
            Err(code) => {
                if let Ok(_wire) = crate::slice_budget::reserve(
                    journal.budget(),
                    crate::slice_budget::ChargeKind::Viewer,
                    256 * 1024,
                ) && let Ok(frame) = fl_host_proto::encode_extension_frame(
                    conn.negotiated_minor(),
                    &fl_host_proto::ExtensionFrame::Notice { code },
                ) {
                    let _ = conn.write_frame(&frame).await;
                }
                break;
            }
        };
        // Max one encoded frame plus bounded base64/part/serde construction, precharged before copies.
        let Ok(_wire) = crate::slice_budget::reserve(
            journal.budget(),
            crate::slice_budget::ChargeKind::Viewer,
            256 * 1024,
        ) else {
            break;
        };
        let lifecycle;
        let (kind, bytes, terminated) = match &record.body {
            crate::reliable::ReliableBody::Line(bytes) => (
                fl_host_proto::ReliableKind::Line,
                bytes.as_slice(),
                bytes.terminated,
            ),
            crate::reliable::ReliableBody::Lifecycle(event, _) => {
                lifecycle = match serde_json::to_vec(event) {
                    Ok(bytes) => bytes,
                    Err(_) => break,
                };
                (
                    fl_host_proto::ReliableKind::Lifecycle,
                    lifecycle.as_slice(),
                    true,
                )
            }
        };
        if !terminated && conn.negotiated_minor() < 2 {
            if let Ok(frame) = fl_host_proto::encode_extension_frame(
                conn.negotiated_minor(),
                &fl_host_proto::ExtensionFrame::Notice {
                    code: fl_host_proto::ExtensionRefusal::Unsupported,
                },
            ) {
                let _ = conn.write_frame(&frame).await;
            }
            break;
        }
        let parts = bytes
            .len()
            .max(1)
            .div_ceil(fl_host_proto::MAX_EXTENSION_PART);
        let mut complete = true;
        for index in 0..parts {
            if !reliable_hold(host, conn, &stream_state, "reliable-before-part", true).await
                || stream_state.cancelled()
                || !host.authority.connection_live(rpc)
            {
                if index == 0
                    && journal.compacted()
                    && !host.stop.load(Ordering::SeqCst)
                    && !conn.peer_closed()
                    && host.authority.connection_live(rpc)
                    && let Ok(frame) = fl_host_proto::encode_extension_frame(
                        conn.negotiated_minor(),
                        &fl_host_proto::ExtensionFrame::Notice {
                            code: fl_host_proto::ExtensionRefusal::SessionFenced,
                        },
                    )
                {
                    let _ = conn.write_frame(&frame).await;
                }
                complete = false;
                break;
            }
            let start = index * fl_host_proto::MAX_EXTENSION_PART;
            let end = bytes.len().min(start + fl_host_proto::MAX_EXTENSION_PART);
            let part =
                fl_host_proto::BoundedBytes::new(bytes[start..end].to_vec()).expect("bounded part");
            let frame = match fl_host_proto::encode_extension_frame(
                conn.negotiated_minor(),
                &fl_host_proto::ExtensionFrame::ReliablePart {
                    terminated,
                    seq: record.seq,
                    kind,
                    more: index + 1 < parts,
                    bytes: part,
                },
            ) {
                Ok(frame) => frame,
                Err(_) => {
                    complete = false;
                    break;
                }
            };
            if index + 1 == parts {
                // Pin once after frame validation, before registering the exact
                // pending final write. Ack shares this instant, never a new TTL.
                let Ok(deadline) = conn.frame_write_deadline(&frame) else {
                    complete = false;
                    break;
                };
                let final_write = match host
                    .authority
                    .begin_final_write(&active, record.seq, deadline)
                {
                    Ok(guard) => guard,
                    Err(_) => {
                        complete = false;
                        break;
                    }
                };
                if !reliable_hold_until(
                    host,
                    conn,
                    &stream_state,
                    "reliable-final-write",
                    true,
                    deadline,
                )
                .await
                {
                    complete = false;
                    break;
                }
                if conn.write_frame_until(&frame, deadline).await.is_err() {
                    complete = false;
                    break;
                }
                if !reliable_hold_until(
                    host,
                    conn,
                    &stream_state,
                    "reliable-before-complete",
                    true,
                    deadline,
                )
                .await
                {
                    complete = false;
                    break;
                }
                if let Some(final_write) = final_write {
                    final_write.complete();
                }
            } else if conn.write_frame(&frame).await.is_err() {
                complete = false;
                break;
            }
            if index + 1 < parts
                && !reliable_hold(host, conn, &stream_state, "reliable-after-part", true).await
            {
                complete = false;
                break;
            }
        }
        if !complete {
            parked = Some(record);
            break;
        }
        drop(record);
    }
    let _ = reliable_hold(host, conn, &stream_state, "reliable-settle", false).await;
    drop(parked);
    drop(active);
}

// Admit the complete operation before allocating its runtime task.
fn spawn_extension_operation<F>(
    host: &HostInner,
    request: XReq,
    operation: impl FnOnce(XReq) -> F,
) -> Result<(), Refusal>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let blocking = matches!(
        request,
        XReq::HostStatus | XReq::ListSessions | XReq::PtyRetentionStatus
    );
    let operation = budgeted_operation(operation(request), &host.reliable_budget)?;
    if blocking {
        // Retention takes spawn_gate across supervisor handshakes and may log
        // to a blocked sink. Move the entire admitted operation, including its
        // snapshot and reply permits, off the async workers. Keeping the
        // budgeted future inside the blocking task also covers queued work and
        // cancellation: no detached capture/result can outlive its charge.
        let handle = host.handle.clone();
        host.handle
            .spawn_blocking(move || handle.block_on(operation));
    } else {
        host.handle.spawn(operation);
    }
    Ok(())
}

// A private cap bounds task/channel descriptors. Metadata charges also bound aggregate owned bodies.
// The inline async operation grew with Ack's cancellation/timeout state. Charge
// its actual monomorphized size before Box::pin and task allocation, separately
// from the reply permit (whose 1024 bytes still cover runtime/channel overhead).
// Boxing keeps pin projection safe and makes both owned allocations explicit.
struct BudgetedOperation<F> {
    future: std::pin::Pin<Box<F>>,
    _metadata: crate::slice_budget::Reservation,
}
impl<F: std::future::Future> std::future::Future for BudgetedOperation<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}
fn budgeted_operation<F: std::future::Future>(
    future: F,
    budget: &Arc<crate::slice_budget::SliceBudget>,
) -> Result<BudgetedOperation<F>, Refusal> {
    let bytes = std::mem::size_of::<F>()
        .checked_add(std::mem::size_of::<BudgetedOperation<F>>())
        .ok_or(Refusal::ResourceLimit)?;
    let metadata =
        crate::slice_budget::reserve(budget, crate::slice_budget::ChargeKind::Metadata, bytes)?;
    Ok(BudgetedOperation {
        future: Box::pin(future),
        _metadata: metadata,
    })
}

const EXTENDED_PENDING_LIMIT: usize = 64;
struct ExtendedPermit {
    count: Arc<std::sync::atomic::AtomicUsize>,
    _metadata: crate::slice_budget::Reservation,
}
impl Drop for ExtendedPermit {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}
struct ExtendedReply {
    response: HostResponse,
    _permit: ExtendedPermit,
    _snapshot: Option<crate::slice_budget::Reservation>,
}
// ListSessions owns two SessionId strings per summary (Session::state and
// StreamId), an explicitly sized record vector, and both response buffers during
// HostConn::respond. The serializer cannot grow beyond MAX_FRAME_PAYLOAD; auth
// allocates exactly payload.len()+5 wire bytes. Charge the full protocol ceiling
// for both before making any summary, including the local buffer descriptors.
fn list_response_storage_bound() -> Option<usize> {
    let records = fl_host_proto::DEFAULT_MAX_SESSIONS
        .checked_mul(std::mem::size_of::<fl_host_proto::SessionSummary>().checked_add(2 * 64)?)?;
    let buffers = fl_host_proto::MAX_FRAME_PAYLOAD
        .checked_mul(2)?
        .checked_add(5)?;
    records
        .checked_add(buffers)?
        .checked_add(std::mem::size_of::<fl_host_proto::Frame>())?
        .checked_add(std::mem::size_of::<Vec<u8>>())
}

#[allow(deprecated)]
fn admit_extension(
    host: &HostInner,
    count: &Arc<std::sync::atomic::AtomicUsize>,
    request: &XReq,
) -> Result<ExtendedPermit, Refusal> {
    // The authenticated decoder bounds the borrowed request. Charge before moving it to an owned continuation.
    let dynamic = match request {
        XReq::SpawnSession { spec, .. } => {
            spec.argv.capacity() * std::mem::size_of::<String>()
                + spec.argv.iter().map(|s| s.capacity()).sum::<usize>()
                + spec.cwd.as_os_str().len() * 2
                + spec
                    .env
                    .iter()
                    .map(|(k, v)| k.capacity() + v.capacity() + 128)
                    .sum::<usize>()
                + spec
                    .env_allow
                    .iter()
                    .map(|s| s.capacity() + 96)
                    .sum::<usize>()
                + 64
        }
        XReq::CheckSessionOwner { hosted_id, .. } => hosted_id.capacity() + 64,
        XReq::SendLine { line, .. } => line.capacity() + 64,
        XReq::Write { bytes, .. } => bytes.as_slice().len() + 64,
        _ => 64,
    };
    let response = match request {
        // Two bounded strings plus JSON (including worst-case escaping and Vec
        // growth) and the wire buffer. The permit lives through the socket write.
        XReq::HostStatus => 8192 + 2 * fl_host_proto::MAX_BUILD_FIELD_BYTES,
        // Bounded identity vector + names, JSON growth and simultaneous wire copy.
        XReq::PtyRetentionStatus => {
            8192 + fl_host_proto::MAX_RECENT_PTY_EVICTIONS
                * (std::mem::size_of::<fl_host_proto::StreamId>() + 64 + 4 * 256)
        }
        XReq::CaptureRendered { .. } => 2 * fl_host_proto::MAX_FRAME_PAYLOAD + 5 + 4096,
        XReq::ListSessions => list_response_storage_bound().ok_or(Refusal::ResourceLimit)?,
        XReq::CaptureOutput { max_bytes, .. } => {
            (*max_bytes as usize).min(fl_host_proto::MAX_RAW_INPUT) + 4096
        }
        _ => 4096,
    };
    let bytes = std::mem::size_of::<XReq>()
        .checked_add(std::mem::size_of::<ExtendedReply>())
        .and_then(|n| n.checked_add(1024))
        .and_then(|n| n.checked_add(dynamic))
        .and_then(|n| n.checked_add(response))
        .ok_or(Refusal::ResourceLimit)?;
    let metadata = crate::slice_budget::reserve(
        &host.reliable_budget,
        crate::slice_budget::ChargeKind::Metadata,
        bytes,
    )?;
    count
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            n.checked_add(1).filter(|n| *n <= EXTENDED_PENDING_LIMIT)
        })
        .map_err(|_| Refusal::Busy)?;
    Ok(ExtendedPermit {
        count: Arc::clone(count),
        _metadata: metadata,
    })
}

// Own the reply and its reservation until the socket write completes or is cancelled.
async fn write_extended_reply(conn: &mut HostConn, reply: ExtendedReply) -> Result<(), HostError> {
    let result = conn.respond(&reply.response).await;
    drop(reply);
    result
}

#[cfg(all(test, unix))]
mod list_response_tests {
    use super::*;
    use crate::slice_budget::{ChargeKind, METADATA, reserve};
    use crate::{OpenOptions, SessionParts};
    use fl_host_proto::{AuthToken, HostConfig, HostEndpoint};
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, Host) {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = HostConfig::new(HostEndpoint::unix(dir.path().join("list.sock")).unwrap());
        config.minor = 1;
        let host = Host::open(OpenOptions::new(config)).unwrap();
        for n in 0..64 {
            let session = Arc::new(Session::new(SessionParts {
                session: SessionId::new(&format!("{n:064}")).unwrap(),
                epoch: u64::MAX,
                vendor_pid: u32::MAX,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            }));
            host.inner.outputs.lock().unwrap().insert(
                (session.session.as_str().to_owned(), session.epoch),
                OutputSession::open(session, Arc::clone(&host.inner.hub)),
            );
        }
        (dir, host)
    }

    // Catches retention handlers blocking all four production runtime workers.
    // Readiness is signalled from inside each scheduled operation immediately
    // before entering the real handler, while the test thread owns spawn_gate.
    #[test]
    fn retention_queries_leave_runtime_workers_available() {
        for request in [
            XReq::HostStatus,
            XReq::ListSessions,
            XReq::PtyRetentionStatus,
        ] {
            let (_dir, host) = fixture();
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            let gate = host.inner.spawn_gate.lock().unwrap();
            for id in 0..4 {
                let request = request.clone();
                let permit = admit_extension(&host.inner, &count, &request).unwrap();
                let inner = Arc::clone(&host.inner);
                let started = started_tx.clone();
                let replies = reply_tx.clone();
                spawn_extension_operation(&host.inner, request, |request| async move {
                    started.send(()).unwrap();
                    let mut snapshot = None;
                    let response = extended(&inner, ConnId(1), request, &mut snapshot)
                        .await
                        .unwrap();
                    let _ = replies.send(ExtendedReply {
                        response: HostResponse::Extended {
                            id: RequestId(id),
                            response,
                        },
                        _permit: permit,
                        _snapshot: snapshot,
                    });
                })
                .unwrap();
            }
            let ready = (0..4).all(|_| started_rx.recv_timeout(Duration::from_secs(5)).is_ok());
            let owner_request = XReq::CheckSessionOwner {
                stream: fl_host_proto::StreamId {
                    host: host.inner.authority.instance().clone(),
                    session: SessionId::new("missing").unwrap(),
                    epoch: fl_host_proto::SessionEpoch(1),
                },
                hosted_id: "missing".into(),
            };
            let permit = admit_extension(&host.inner, &count, &owner_request).unwrap();
            let inner = Arc::clone(&host.inner);
            let (owner_tx, owner_rx) = std::sync::mpsc::channel();
            spawn_extension_operation(&host.inner, owner_request, |request| async move {
                let mut snapshot = None;
                let response = extended(&inner, ConnId(1), request, &mut snapshot)
                    .await
                    .unwrap();
                let _ = owner_tx.send(ExtendedReply {
                    response: HostResponse::Extended {
                        id: RequestId(4),
                        response,
                    },
                    _permit: permit,
                    _snapshot: snapshot,
                });
            })
            .unwrap();
            let owner = owner_rx.recv_timeout(Duration::from_secs(1));
            // Always release the lock before asserting so a failing regression
            // cannot strand the runtime or the host's shutdown path.
            drop(gate);
            for _ in 0..4 {
                drop(reply_rx.recv_timeout(Duration::from_secs(5)).unwrap());
            }
            assert!(
                ready,
                "all four retention operations must enter the handler"
            );
            assert!(
                matches!(
                    owner.as_ref().map(|reply| &reply.response),
                    Ok(HostResponse::Extended {
                        response: XResp::SessionOwner { matches: false },
                        ..
                    })
                ),
                "{request:?}: ownership check stalled behind retention maintenance"
            );
            drop(owner);
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
    }

    fn rendered_fixture() -> (tempfile::TempDir, Host, fl_host_proto::StreamId) {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = HostConfig::new(HostEndpoint::unix(dir.path().join("screen.sock")).unwrap());
        let host = Host::open(OpenOptions::new(config)).unwrap();
        let stream = fl_host_proto::StreamId {
            host: host.inner.authority.instance().clone(),
            session: SessionId::new("screen").unwrap(),
            epoch: fl_host_proto::SessionEpoch(1),
        };
        let journal = crate::reliable::ReliableJournal::new_with_mode(
            Arc::clone(&host.inner.reliable_budget),
            stream.clone(),
            true,
        )
        .unwrap();
        let mut projection = crate::projection::Projection::new(
            Arc::clone(journal.budget()),
            fl_host_proto::PtyOptions {
                cols: 256,
                rows: 32,
                scrollback_rows: 0,
            },
        )
        .unwrap();
        projection.process(&[b'\\'; 8000]);
        let session = Arc::new(Session::with_projection(
            SessionParts {
                session: stream.session.clone(),
                epoch: 1,
                vendor_pid: 0,
                vendor_start: 0,
                supervisor_pid: 0,
                supervisor_start: 0,
                pgid: 0,
                control_inode: 0,
                control: None,
                stdin: None,
            },
            Arc::clone(&journal),
            projection,
            None,
        ));
        let writer = WriterHandle::start(Arc::clone(&session), None);
        host.inner.authority.connect(ConnId(1)).unwrap();
        host.inner.authority.register(
            host.inner.authority.prepare_epoch(journal).unwrap(),
            &session,
            &writer,
        );
        host.inner
            .writers
            .lock()
            .unwrap()
            .insert("screen".into(), writer);
        host.inner.outputs.lock().unwrap().insert(
            ("screen".into(), 1),
            OutputSession::open(session, Arc::clone(&host.inner.hub)),
        );
        (dir, host, stream)
    }

    // Catches unbounded pending response admission and count/metadata leaks on refusal.
    #[test]
    fn rendered_pending_admission_and_resize_control_queue_are_bounded() {
        let (_dir, host, stream) = rendered_fixture();
        let budget = &host.inner.reliable_budget;
        let baseline = budget.used(ChargeKind::Metadata);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request = XReq::CaptureRendered {
            stream: stream.clone(),
        };
        let mut permits = Vec::new();
        loop {
            match admit_extension(&host.inner, &count, &request) {
                Ok(permit) => permits.push(permit),
                Err(Refusal::ResourceLimit) => break,
                other => panic!("admission: {}", other.is_ok()),
            }
            assert!(permits.len() <= 16);
        }
        assert_eq!(count.load(Ordering::SeqCst), permits.len());
        drop(permits);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(budget.used(ChargeKind::Metadata), baseline);
        let writer = host
            .inner
            .writers
            .lock()
            .unwrap()
            .get("screen")
            .unwrap()
            .clone();
        let mut replies = Vec::new();
        for _ in 0..32 {
            replies.push(
                writer
                    .resize(
                        ConnId(1),
                        stream.clone(),
                        (ControllerLease(1), ControllerLease(1)),
                        (80, 24),
                        &host.inner.authority,
                        budget,
                    )
                    .unwrap(),
            );
        }
        let queued = budget.used(ChargeKind::Metadata);
        assert!(queued > baseline);
        assert!(matches!(
            writer.resize(
                ConnId(1),
                stream,
                (ControllerLease(1), ControllerLease(1)),
                (80, 24),
                &host.inner.authority,
                budget
            ),
            Err(Refusal::Busy)
        ));
        assert_eq!(budget.used(ChargeKind::Metadata), queued);
        writer.shutdown();
        drop(replies);
        assert_eq!(budget.used(ChargeKind::Metadata), baseline);
    }

    // Catches allocating snapshot/serialization bodies without their separate admission.
    #[test]
    fn rendered_capture_refuses_exhausted_snapshot_or_metadata_before_allocation() {
        let (_dir, host, stream) = rendered_fixture();
        let budget = &host.inner.reliable_budget;
        let base = budget.used(ChargeKind::Metadata);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let filler = reserve(budget, ChargeKind::Metadata, METADATA - base - 70_000).unwrap();
        assert!(matches!(
            admit_extension(
                &host.inner,
                &count,
                &XReq::CaptureRendered {
                    stream: stream.clone()
                }
            ),
            Err(Refusal::ResourceLimit)
        ));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(filler);
        let _filler = reserve(budget, ChargeKind::Snapshot, 32 * 1024 * 1024).unwrap();
        let mut reservation = None;
        assert_eq!(
            host.inner.handle.block_on(extended(
                &host.inner,
                ConnId(1),
                XReq::CaptureRendered { stream },
                &mut reservation
            )),
            Err(Refusal::ResourceLimit)
        );
        assert!(reservation.is_none());
        assert_eq!(budget.used(ChargeKind::Snapshot), 32 * 1024 * 1024);
    }

    #[test]
    fn rendered_snapshot_and_buffers_survive_pending_write_until_retirement() {
        for retirement in ["cancel", "closed", "complete"] {
            pending_write_retirement(retirement, true);
        }
    }

    // Catches accepting the old 64-KiB allowance when concurrent response storage cannot fit.
    #[test]
    fn list_response_refuses_before_uncovered_allocation_near_metadata_limit() {
        let (_dir, host) = fixture();
        let budget = &host.inner.reliable_budget;
        let used = budget.used(ChargeKind::Metadata);
        let _filler = reserve(budget, ChargeKind::Metadata, METADATA - used - 70_000).unwrap();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(
            matches!(
                admit_extension(&host.inner, &count, &XReq::ListSessions),
                Err(Refusal::ResourceLimit)
            ),
            "must refuse before creating the owned summaries and serialization buffers"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(budget.used(ChargeKind::Metadata), METADATA - 70_000);
    }

    // Catches undercharging after real serialization, early permit retirement, and cancellation leaks.
    #[test]
    fn list_response_charge_survives_allocated_pending_socket_write() {
        for retirement in ["cancel", "closed", "complete"] {
            pending_write_retirement(retirement, false);
        }
    }

    fn pending_write_retirement(retirement: &str, rendered: bool) {
        let (_dir, host, stream) = if rendered {
            let (dir, host, stream) = rendered_fixture();
            (dir, host, Some(stream))
        } else {
            let (dir, host) = fixture();
            (dir, host, None)
        };
        host.inner.handle.block_on(async {
            let endpoint = host.endpoint();
            let HostEndpoint::Unix(path) = endpoint else { unreachable!() };
            let token_path = path.with_file_name(endpoint.token_file_name());
            let token = AuthToken::new(std::fs::read(token_path).unwrap().try_into().unwrap());
            let mut client = tokio::net::UnixStream::connect(path).await.unwrap();
            fl_host_proto::client_handshake(&mut client, &endpoint.identity_bytes(), &token,
                fl_host_proto::PROTO_MAJOR, 1,
                fl_host_proto::Deadlines { handshake: Duration::from_secs(5), frame: Duration::from_secs(5), write: Duration::from_secs(5) }, None).await.unwrap();
            let mut listener = host.inner.listener.lock().unwrap().take().unwrap();
            let mut conn = listener.accept().await.unwrap();
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let budget = &host.inner.reliable_budget;
            let used = budget.used(ChargeKind::Metadata);
            let _filler = reserve(budget, ChargeKind::Metadata, METADATA - used - 600_000).unwrap();
            let baseline = budget.used(ChargeKind::Metadata);
            let snapshot_baseline = budget.used(ChargeKind::Snapshot);
            let mut parked = false;
            let mut wire_bytes = 0;
            // The peer never reads. Completed writes fill the native socket; the next
            // manual poll must stop inside the actual response write, after encoding.
            for id in 1..=128 {
                let request = stream.as_ref().map(|stream|XReq::CaptureRendered{stream:stream.clone()}).unwrap_or(XReq::ListSessions);
                let permit = admit_extension(&host.inner, &count, &request).unwrap();
                let mut snapshot = None;
                let response = HostResponse::Extended { id: RequestId(id), response: extended(&host.inner, ConnId(1), request, &mut snapshot).await.unwrap() };
                let owned_records = match &response {
                    HostResponse::Extended {response:XResp::Sessions{records},..} => {
                        assert_eq!(records.len(),64);
                        records.capacity()*std::mem::size_of::<fl_host_proto::SessionSummary>()+64*128
                    }
                    HostResponse::Extended {response:XResp::RenderedCapture{screen},..} => {
                        assert_eq!(screen.contents.len(),8000);
                        assert!(snapshot.is_some());
                        screen.contents.capacity()
                    }
                    _ => unreachable!(),
                };
                let encoded = fl_host_proto::encode_response_for_minor(1, &response).unwrap();
                wire_bytes += encoded.payload.len() + 5;
                let concurrent_storage = owned_records + encoded.payload.capacity() + encoded.payload.len() + 5
                    + std::mem::size_of::<ExtendedReply>()
                    + std::mem::size_of::<fl_host_proto::Frame>()
                    + std::mem::size_of::<Vec<u8>>();
                drop(encoded);
                let charged = budget.used(ChargeKind::Metadata) - baseline;
                let mut write = Box::pin(write_extended_reply(&mut conn, ExtendedReply { response, _permit: permit, _snapshot: snapshot }));
                let mut cx = Context::from_waker(Waker::noop());
                match write.as_mut().poll(&mut cx) {
                    Poll::Ready(result) => { result.unwrap(); drop(write); assert_eq!(budget.used(ChargeKind::Metadata), baseline); }
                    Poll::Pending => {
                        parked = true;
                        eprintln!("{retirement}: parked allocated response {id}, charged={charged}, concurrent_storage={concurrent_storage}");
                        assert_eq!(count.load(Ordering::SeqCst), 1);
                        assert_eq!(budget.used(ChargeKind::Metadata), baseline + charged);
                        let snapshot_charge = budget.used(ChargeKind::Snapshot) - snapshot_baseline;
                        assert_eq!(snapshot_charge > 0, rendered);
                        assert!(charged + snapshot_charge >= concurrent_storage, "parked response owns at least {concurrent_storage} bytes but charges only {charged}");
                        match retirement {
                            "cancel" => drop(write),
                            "closed" => {
                                // Peer shutdown must retire both transport buffers.
                                // Shut down both halves through the native socket.
                                use std::os::fd::AsRawFd;
                                // SAFETY: the live socket owns this fd; shutdown does not close it.
                                assert_eq!(unsafe { libc::shutdown(client.as_raw_fd(), libc::SHUT_RDWR) }, 0);
                                assert!(write.await.is_err());
                            }
                            "complete" => {
                                use tokio::io::AsyncReadExt;
                                let mut received = vec![0u8; wire_bytes];
                                let (read, written) = tokio::time::timeout(Duration::from_secs(5), async {
                                    tokio::join!(client.read_exact(&mut received), write)
                                }).await.expect("response drain exceeded the production write deadline");
                                read.unwrap();
                                written.unwrap();
                                // Decode every real response after transport retirement.
                                let mut bytes = std::io::Cursor::new(received);
                                for _ in 0..id {
                                    let frame = fl_host_proto::read_frame(&mut bytes).unwrap();
                                    match fl_host_proto::decode_response_for_minor(1,&frame).unwrap() {
                                        HostResponse::Extended {response:XResp::Sessions{records},..} => assert_eq!(records.len(),64),
                                        HostResponse::Extended {response:XResp::RenderedCapture{screen},..} => assert_eq!(screen.contents.len(),8000),
                                        other => panic!("response {other:?}"),
                                    }
                                }
                            }
                            _ => unreachable!(),
                        }
                        assert_eq!(count.load(Ordering::SeqCst), 0);
                        assert_eq!(budget.used(ChargeKind::Metadata), baseline);
                        assert_eq!(budget.used(ChargeKind::Snapshot), snapshot_baseline);
                        break;
                    }
                }
            }
            assert!(parked, "native socket must reach a pending allocated write");
            drop(client);
            drop(conn);
            *host.inner.listener.lock().unwrap() = Some(listener);
        });
    }
}

#[cfg(test)]
mod operation_budget_tests {
    use super::*;
    use crate::slice_budget::{ChargeKind, METADATA, SliceBudget, reserve};
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    // Catches growing task futures relying on the old arbitrary 1024-byte allowance.
    #[test]
    fn actual_operation_future_is_precharged_until_cancellation() {
        let budget = SliceBudget::new();
        let base = budget.used(ChargeKind::Metadata);
        let bytes = [0u8; 16_384];
        let future = async move {
            std::future::pending::<()>().await;
            std::hint::black_box(bytes);
        };
        let future_bytes = std::mem::size_of_val(&future);
        assert!(future_bytes >= 16_384);
        let mut operation = Box::pin(budgeted_operation(future, &budget).unwrap());
        let charged = budget.used(ChargeKind::Metadata) - base;
        assert!(charged >= future_bytes + std::mem::size_of_val(&*operation));
        assert!(matches!(
            operation
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(budget.used(ChargeKind::Metadata), base + charged);
        drop(operation);
        assert_eq!(budget.used(ChargeKind::Metadata), base);
        let _filler = reserve(&budget, ChargeKind::Metadata, METADATA - base - 1024).unwrap();
        let future = async move {
            std::future::pending::<()>().await;
            std::hint::black_box(bytes);
        };
        assert!(matches!(
            budgeted_operation(future, &budget),
            Err(Refusal::ResourceLimit)
        ));
        assert_eq!(budget.used(ChargeKind::Metadata), METADATA - 1024);
    }
}
