fn main() {
    if touchbar_plugin_supervisor::run_secret_transport_helper().is_err() {
        std::process::exit(1);
    }
}
