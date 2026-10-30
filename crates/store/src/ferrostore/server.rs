//! One `FerroStore` serving many clients over a Unix socket.
//!
//! The server process is the only opener of the `.fls` file. Clients speak
//! length-prefixed JSON and never take `flock`.

use super::layout::backend;
use super::{FerroStore, MqRecord, lock_mutex};
use fl_core::StoreError;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Accept is nonblocking. Drop waits at most one pause to observe the flag.
const ACCEPT_PAUSE: Duration = Duration::from_millis(20);
/// Forwarders wake on this interval so a closed connection can join them.
const FORWARD_PAUSE: Duration = Duration::from_millis(50);

struct ServerState {
    store: Arc<FerroStore>,
    shutdown: AtomicBool,
    conns: Mutex<HashMap<u64, UnixStream>>,
    next_conn: AtomicU64,
}

struct ConnReg {
    state: Arc<ServerState>,
    id: u64,
}

impl Drop for ConnReg {
    fn drop(&mut self) {
        lock_mutex(&self.state.conns).remove(&self.id);
    }
}

/// Accept thread, shutdown flag, and the socket path this server bound.
pub struct FerroStoreServer {
    path: PathBuf,
    inode: u64,
    unlink: bool,
    state: Arc<ServerState>,
    accept: Option<JoinHandle<()>>,
}

impl FerroStoreServer {
    /// Bind a Unix socket and serve `store`.
    ///
    /// Moves `store` in and does not open its path again. If `path` exists and
    /// is a socket, it is unlinked first. If it exists and is not a socket,
    /// this returns [`StoreError::Backend`].
    pub fn bind_unix(path: impl AsRef<Path>, store: FerroStore) -> Result<Self, StoreError> {
        let path = path.as_ref();
        prepare_bind_path(path)?;
        let listener = UnixListener::bind(path)
            .map_err(|err| backend(format!("bind {}: {err}", path.display())))?;
        if let Err(err) = listener.set_nonblocking(true) {
            drop(listener);
            let _ = fs::remove_file(path);
            return Err(backend(format!("set socket nonblocking: {err}")));
        }
        let inode = match fs::symlink_metadata(path) {
            Ok(meta) => meta.ino(),
            Err(err) => {
                drop(listener);
                let _ = fs::remove_file(path);
                return Err(backend(format!("stat {}: {err}", path.display())));
            }
        };
        let state = Arc::new(ServerState {
            store: Arc::new(store),
            shutdown: AtomicBool::new(false),
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        });
        let accept_state = Arc::clone(&state);
        let accept = match thread::Builder::new()
            .name("fls-accept".to_string())
            .spawn(move || accept_loop(listener, accept_state))
        {
            Ok(handle) => handle,
            Err(err) => {
                unlink_bound(path, inode);
                return Err(backend(format!("start accept thread: {err}")));
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            inode,
            unlink: true,
            state,
            accept: Some(accept),
        })
    }
}

#[cfg(test)]
impl FerroStoreServer {
    pub(super) fn store(&self) -> &FerroStore {
        &self.state.store
    }
}

impl Drop for FerroStoreServer {
    fn drop(&mut self) {
        // Stop accept and connection readers before joining. Unlink only the
        // inode this server bound, so a replacement socket is left alone.
        self.state.shutdown.store(true, Ordering::SeqCst);
        shutdown_conns(&self.state);
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        if self.unlink {
            unlink_bound(&self.path, self.inode);
        }
    }
}

fn prepare_bind_path(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path)
            .map_err(|err| backend(format!("unlink {}: {err}", path.display()))),
        Ok(_) => Err(backend(format!(
            "{} exists and is not a socket",
            path.display()
        ))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(backend(format!("stat {}: {err}", path.display()))),
    }
}

fn unlink_bound(path: &Path, inode: u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.ino() == inode && meta.file_type().is_socket() {
        let _ = fs::remove_file(path);
    }
}

fn shutdown_conns(state: &ServerState) {
    let conns = std::mem::take(&mut *lock_mutex(&state.conns));
    for (_, stream) in conns {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

fn accept_loop(listener: UnixListener, state: Arc<ServerState>) {
    let mut handlers = Vec::new();
    loop {
        reap(&mut handlers);
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if state.shutdown.load(Ordering::SeqCst) {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                let Ok(watch) = stream.try_clone() else {
                    continue;
                };
                let id = state.next_conn.fetch_add(1, Ordering::Relaxed);
                lock_mutex(&state.conns).insert(id, watch);
                let conn_state = Arc::clone(&state);
                match thread::Builder::new()
                    .name("fls-conn".to_string())
                    .spawn(move || serve_connection(stream, conn_state, id))
                {
                    Ok(handle) => handlers.push(handle),
                    Err(_) => {
                        lock_mutex(&state.conns).remove(&id);
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_PAUSE);
            }
            Err(err) if accept_again(&err) => {}
            Err(_) => break,
        }
    }
    state.shutdown.store(true, Ordering::SeqCst);
    shutdown_conns(&state);
    for handle in handlers {
        let _ = handle.join();
    }
}

fn accept_again(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    )
}

fn reap(handlers: &mut Vec<JoinHandle<()>>) {
    let pending = std::mem::take(handlers);
    for handle in pending {
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            handlers.push(handle);
        }
    }
}

fn serve_connection(mut read: UnixStream, state: Arc<ServerState>, conn_id: u64) {
    let _reg = ConnReg {
        state: Arc::clone(&state),
        id: conn_id,
    };
    let Ok(writer) = read.try_clone() else {
        return;
    };
    let write = Arc::new(Mutex::new(writer));
    let alive = Arc::new(AtomicBool::new(true));
    let mut subscribed = HashSet::new();
    let mut forwarders = Vec::new();
    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let frame = match read_frame(&mut read) {
            Ok(frame) => frame,
            Err(_) => break,
        };
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            break;
        };
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = if method == "subscribe_mq" {
            subscribe_topic(
                &state.store,
                &params,
                &write,
                &alive,
                &mut subscribed,
                &mut forwarders,
            )
            .map(|()| Value::Null)
        } else {
            dispatch(&state.store, method, &params)
        };
        // An unknown method is an error response. The connection stays up.
        if send_result(&write, id, result).is_err() {
            break;
        }
    }
    alive.store(false, Ordering::SeqCst);
    for handle in forwarders {
        let _ = handle.join();
    }
}

fn subscribe_topic(
    store: &FerroStore,
    params: &Value,
    write: &Arc<Mutex<UnixStream>>,
    alive: &Arc<AtomicBool>,
    subscribed: &mut HashSet<String>,
    forwarders: &mut Vec<JoinHandle<()>>,
) -> Result<(), StoreError> {
    let topic = param_str(params, "topic")?.to_string();
    if subscribed.contains(&topic) {
        return Ok(());
    }
    // One in-process subscription per topic. It does not replay history.
    let rx = store.subscribe_mq(&topic);
    let topic_for_thread = topic.clone();
    let write = Arc::clone(write);
    let alive = Arc::clone(alive);
    let handle = thread::Builder::new()
        .name("fls-mq".to_string())
        .spawn(move || forward_mq(rx, topic_for_thread, write, alive))
        .map_err(|err| backend(format!("start mq forwarder: {err}")))?;
    subscribed.insert(topic);
    forwarders.push(handle);
    Ok(())
}

fn forward_mq(
    rx: Receiver<MqRecord>,
    topic: String,
    write: Arc<Mutex<UnixStream>>,
    alive: Arc<AtomicBool>,
) {
    loop {
        if !alive.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(FORWARD_PAUSE) {
            Ok(record) => {
                let frame = json!({
                    "method": "mq",
                    "params": {
                        "topic": topic,
                        "record": record_value(record),
                    }
                });
                let mut stream = lock_mutex(&write);
                if write_frame(&mut stream, &frame).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn dispatch(store: &FerroStore, method: &str, params: &Value) -> Result<Value, StoreError> {
    match method {
        "put_doc" => {
            store.put_doc(
                param_str(params, "collection")?,
                param_str(params, "id")?,
                &param_value(params, "doc")?,
            )?;
            Ok(Value::Null)
        }
        "get_doc" => Ok(store
            .get_doc(param_str(params, "collection")?, param_str(params, "id")?)?
            .unwrap_or(Value::Null)),
        "delete_doc" => {
            store.delete_doc(param_str(params, "collection")?, param_str(params, "id")?)?;
            Ok(Value::Null)
        }
        "query_by_field" => {
            let ids = store.query_by_field(
                param_str(params, "collection")?,
                param_str(params, "field")?,
                param_str(params, "value")?,
            )?;
            Ok(Value::Array(ids.into_iter().map(Value::String).collect()))
        }
        "search_bm25" => {
            let hits = store.search_bm25(
                param_str(params, "collection")?,
                param_str(params, "query")?,
                param_usize(params, "limit")?,
            )?;
            Ok(Value::Array(
                hits.into_iter()
                    .map(|(id, score)| json!([id, score]))
                    .collect(),
            ))
        }
        "publish_mq" => {
            let seq = store.publish_mq(
                param_str(params, "topic")?,
                param_str(params, "key")?,
                &param_value(params, "payload")?,
            )?;
            Ok(Value::from(seq))
        }
        "poll_mq_group" => {
            let records = store.poll_mq_group(
                param_str(params, "topic")?,
                param_str(params, "group")?,
                param_usize(params, "limit")?,
            )?;
            Ok(Value::Array(
                records.into_iter().map(record_value).collect(),
            ))
        }
        "ack_mq" => {
            store.ack_mq(
                param_str(params, "topic")?,
                param_str(params, "group")?,
                param_u64(params, "seq")?,
            )?;
            Ok(Value::Null)
        }
        "checkpoint" => {
            store.checkpoint()?;
            Ok(Value::Null)
        }
        _ => Err(backend(format!("no such method `{method}`"))),
    }
}

fn send_result(
    write: &Mutex<UnixStream>,
    id: u64,
    result: Result<Value, StoreError>,
) -> Result<(), StoreError> {
    let body = match result {
        Ok(result) => json!({"id": id, "ok": true, "result": result}),
        Err(err) => json!({"id": id, "ok": false, "error": err.to_string()}),
    };
    let mut stream = lock_mutex(write);
    write_frame(&mut stream, &body)
}

fn record_value(record: MqRecord) -> Value {
    json!({
        "seq": record.seq,
        "key": record.key,
        "payload": record.payload,
    })
}

fn param_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| backend(format!("missing string param `{key}`")))
}

fn param_u64(params: &Value, key: &str) -> Result<u64, StoreError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| backend(format!("missing integer param `{key}`")))
}

fn param_usize(params: &Value, key: &str) -> Result<usize, StoreError> {
    usize::try_from(param_u64(params, key)?)
        .map_err(|_| backend(format!("param `{key}` does not fit")))
}

fn param_value(params: &Value, key: &str) -> Result<Value, StoreError> {
    params
        .get(key)
        .cloned()
        .ok_or_else(|| backend(format!("missing param `{key}`")))
}

fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(value).map_err(|err| backend(format!("encode frame: {err}")))?;
    let len = u32::try_from(bytes.len()).map_err(|_| backend("frame length exceeds u32"))?;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(backend("frame length is outside 1..=16MiB"));
    }
    stream.write_all(&len.to_be_bytes()).map_err(backend)?;
    stream.write_all(&bytes).map_err(backend)?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Value, StoreError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(backend)?;
    let len = u32::from_be_bytes(len_buf);
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(backend("frame length is outside 1..=16MiB"));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).map_err(backend)?;
    let text = std::str::from_utf8(&buf).map_err(|_| backend("frame is not utf-8"))?;
    serde_json::from_str(text).map_err(|_| backend("frame is not json"))
}

struct RemoteShared {
    write: Mutex<UnixStream>,
    pending: Mutex<HashMap<u64, Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    subs: Mutex<HashMap<String, Vec<Sender<MqRecord>>>>,
}

pub(super) struct RemoteClient {
    shared: Arc<RemoteShared>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl RemoteClient {
    pub(super) fn call(&self, method: &str, params: Value) -> Result<Value, StoreError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock_mutex(&self.shared.pending).insert(id, tx);
        let request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        // Release the write lock before waiting. The reader must run, and a
        // response must not need this task to still hold the mutex.
        {
            let mut stream = lock_mutex(&self.shared.write);
            if let Err(err) = write_frame(&mut stream, &request) {
                lock_mutex(&self.shared.pending).remove(&id);
                return Err(err);
            }
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(backend(message)),
            Err(RecvTimeoutError::Timeout) => {
                lock_mutex(&self.shared.pending).remove(&id);
                Err(backend("timed out waiting for the FerroStore server"))
            }
            Err(RecvTimeoutError::Disconnected) => Err(backend("FerroStore connection closed")),
        }
    }

    pub(super) fn call_unit(&self, method: &str, params: Value) -> Result<(), StoreError> {
        let _ = self.call(method, params)?;
        Ok(())
    }

    /// JSON null is an absent document. A stored null is not distinct on the wire.
    pub(super) fn call_option(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Option<Value>, StoreError> {
        let value = self.call(method, params)?;
        if value.is_null() {
            Ok(None)
        } else {
            Ok(Some(value))
        }
    }

    pub(super) fn call_strings(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Vec<String>, StoreError> {
        let value = self.call(method, params)?;
        let items = value
            .as_array()
            .ok_or_else(|| backend("result is not an array"))?;
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(ToString::to_string)
                    .ok_or_else(|| backend("result id is not a string"))
            })
            .collect()
    }

    pub(super) fn call_pairs(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let value = self.call(method, params)?;
        let items = value
            .as_array()
            .ok_or_else(|| backend("result is not an array"))?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let pair = item
                .as_array()
                .ok_or_else(|| backend("search hit is not a pair"))?;
            if pair.len() != 2 {
                return Err(backend("search hit is not a pair"));
            }
            let id = pair[0]
                .as_str()
                .ok_or_else(|| backend("search hit id is not a string"))?
                .to_string();
            let score = pair[1]
                .as_f64()
                .ok_or_else(|| backend("search hit score is not a number"))?;
            out.push((id, score));
        }
        Ok(out)
    }

    pub(super) fn call_u64(&self, method: &str, params: Value) -> Result<u64, StoreError> {
        let value = self.call(method, params)?;
        value
            .as_u64()
            .ok_or_else(|| backend(format!("{method} result is not a u64")))
    }

    pub(super) fn call_records(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Vec<MqRecord>, StoreError> {
        let value = self.call(method, params)?;
        let items = value
            .as_array()
            .ok_or_else(|| backend("result is not an array"))?;
        items.iter().map(parse_record).collect()
    }

    pub(super) fn subscribe_topic(&self, topic: &str) -> Receiver<MqRecord> {
        let (tx, rx) = mpsc::channel();
        lock_mutex(&self.shared.subs)
            .entry(topic.to_string())
            .or_default()
            .push(tx);
        // The sender is registered first so a push that arrives with the
        // response is delivered. The method returns only the receiver.
        let _ = self.call("subscribe_mq", json!({"topic": topic}));
        rx
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        let stream = lock_mutex(&self.shared.write);
        let _ = stream.shutdown(Shutdown::Both);
        drop(stream);
        if let Some(handle) = lock_mutex(&self.reader).take() {
            let _ = handle.join();
        }
    }
}

pub(super) fn connect_unix(path: &str) -> Result<RemoteClient, StoreError> {
    let stream =
        UnixStream::connect(path).map_err(|err| backend(format!("connect {path}: {err}")))?;
    let writer = stream
        .try_clone()
        .map_err(|err| backend(format!("clone unix stream: {err}")))?;
    let shared = Arc::new(RemoteShared {
        write: Mutex::new(writer),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        subs: Mutex::new(HashMap::new()),
    });
    let reader_shared = Arc::clone(&shared);
    let reader = thread::Builder::new()
        .name("fls-client".to_string())
        .spawn(move || reader_loop(stream, reader_shared))
        .map_err(|err| backend(format!("start client reader: {err}")))?;
    Ok(RemoteClient {
        shared,
        reader: Mutex::new(Some(reader)),
    })
}

fn reader_loop(mut stream: UnixStream, shared: Arc<RemoteShared>) {
    while let Ok(frame) = read_frame(&mut stream) {
        if frame.get("id").is_none() && frame.get("method").and_then(Value::as_str) == Some("mq") {
            deliver_push(&shared, &frame);
            continue;
        }
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let outcome = if frame.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(frame.get("result").cloned().unwrap_or(Value::Null))
        } else {
            let message = frame
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("FerroStore request failed");
            Err(message.to_string())
        };
        if let Some(sender) = lock_mutex(&shared.pending).remove(&id) {
            let _ = sender.send(outcome);
        }
    }
    fail_pending(&shared, "FerroStore connection closed");
}

fn deliver_push(shared: &RemoteShared, frame: &Value) {
    let Some(params) = frame.get("params") else {
        return;
    };
    let Some(topic) = params.get("topic").and_then(Value::as_str) else {
        return;
    };
    let Some(record_value) = params.get("record") else {
        return;
    };
    let Ok(record) = parse_record(record_value) else {
        return;
    };
    let mut subs = lock_mutex(&shared.subs);
    let Some(senders) = subs.get_mut(topic) else {
        return;
    };
    senders.retain(|sender| sender.send(record.clone()).is_ok());
    if senders.is_empty() {
        subs.remove(topic);
    }
}

fn parse_record(value: &Value) -> Result<MqRecord, StoreError> {
    let seq = value
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| backend("mq record is missing seq"))?;
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("mq record is missing key"))?
        .to_string();
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
    Ok(MqRecord { seq, key, payload })
}

fn fail_pending(shared: &RemoteShared, message: &str) {
    let pending = std::mem::take(&mut *lock_mutex(&shared.pending));
    for (_, sender) in pending {
        let _ = sender.send(Err(message.to_string()));
    }
}
