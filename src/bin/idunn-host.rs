fn main() -> anyhow::Result<()> {
    idunn_daemon::host::run(std::env::args().skip(1))
}
