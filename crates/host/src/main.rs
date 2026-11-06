fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("--session-supervisor") => fl_host::session_supervisor(),
        Some("--containment-worker") => fl_host::containment_worker(),
        Some("--socket") => {
            let path = args.next().unwrap_or_default();
            match fl_host::socket_endpoint(&path) {
                Ok(endpoint) => fl_host::serve(fl_host_proto::HostConfig::new(endpoint)),
                Err(err) => Err(err),
            }
        }
        _ => match fl_host_proto::HostEndpoint::discover() {
            Ok(endpoint) => fl_host::serve(fl_host_proto::HostConfig::new(endpoint)),
            Err(err) => Err(err),
        },
    };
    if let Err(err) = result {
        eprintln!("fl-host: {err}");
        std::process::exit(1);
    }
}
