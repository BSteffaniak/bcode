use bcode_plugin_sdk::AuthRegistrar;

fn main() {
    let registrar = AuthRegistrar::default();
    std::thread::spawn(move || drop(registrar));
}
