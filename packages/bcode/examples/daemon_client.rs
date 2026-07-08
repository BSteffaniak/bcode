use bcode::{Bcode, BcodeClient, BcodeMode, DaemonAvailability};

fn main() {
    let client =
        BcodeClient::default_endpoint().with_daemon_availability(DaemonAvailability::AutoStart);
    let bcode = Bcode::builder().daemon_client(client).build();

    assert_eq!(bcode.mode(), BcodeMode::Daemon);
    println!(
        "daemon client configured: {}",
        bcode.daemon_client().is_some()
    );
}
