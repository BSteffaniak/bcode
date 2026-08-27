use bcode_plugin_sdk::CommandRegistrar;

fn main() {
    let registrar = CommandRegistrar::default();
    std::thread::spawn(move || drop(registrar));
}
