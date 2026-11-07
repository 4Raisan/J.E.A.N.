fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--session") {
        return Err(std::io::Error::other("expected --session"));
    }
    let session = args
        .next()
        .ok_or_else(|| std::io::Error::other("missing session"))?;
    jean_core::task_worker::serve(session)
}
