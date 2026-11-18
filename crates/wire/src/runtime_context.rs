//! Scoped runtime selection for callers that already own a lifecycle fence.
//! The scope never mutates the environment, and cannot move to another thread.
use fl_host_proto::{HostEndpoint, HostError};
use std::{
    cell::RefCell,
    marker::PhantomData,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};
#[derive(Clone)]
struct Selection {
    endpoint: HostEndpoint,
    tmux: Option<PathBuf>,
}
thread_local! { static PIN: RefCell<Option<Selection>> = const { RefCell::new(None) }; }
pub struct RuntimeScope(Option<Selection>, PhantomData<Rc<()>>);
impl Drop for RuntimeScope {
    fn drop(&mut self) {
        PIN.with(|pin| *pin.borrow_mut() = self.0.take());
    }
}
/// Caller must own the corresponding native and optional tmux lifecycle fences.
pub fn pin(endpoint: HostEndpoint, tmux: Option<PathBuf>) -> RuntimeScope {
    RuntimeScope(
        PIN.with(|pin| pin.replace(Some(Selection { endpoint, tmux }))),
        PhantomData,
    )
}
pub fn pinned_endpoint() -> Option<HostEndpoint> {
    PIN.with_borrow(|pin| pin.as_ref().map(|pin| pin.endpoint.clone()))
}
pub fn host_endpoint() -> Result<HostEndpoint, HostError> {
    pinned_endpoint().map_or_else(HostEndpoint::discover, Ok)
}
/// A native-only owner cannot fall back to an unfenced tmux server.
pub fn tmux_command(binary: &Path) -> Result<Command, String> {
    let mut command = Command::new(binary);
    PIN.with_borrow(|pin| {
        if let Some(pin) = pin {
            let socket = pin
                .tmux
                .as_ref()
                .ok_or("tmux runtime is not covered by the lifecycle fence")?;
            command.arg("-S").arg(socket);
        }
        Ok(command)
    })
}
