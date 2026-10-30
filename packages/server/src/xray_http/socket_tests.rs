use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(start_paused = true)]
async fn socket_slots_retire_and_header_trickles_do_not_extend_deadline() {
    let raw = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = raw.local_addr().unwrap();
    let sockets = Arc::new(Semaphore::new(1));
    let mut listener = BoundedListener {
        inner: raw,
        sockets: Arc::clone(&sockets),
    };
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut server, _) = axum::serve::Listener::accept(&mut listener).await;
    assert_eq!(sockets.available_permits(), 0);
    client.write_all(b"P").await.unwrap();
    let mut buffer = [0; 1];
    server.read_exact(&mut buffer).await.unwrap();
    assert_eq!(buffer, [b'P']);
    tokio::time::advance(REQUEST_TIMEOUT + Duration::from_millis(1)).await;
    assert_eq!(
        server.read(&mut buffer).await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    drop(server);
    assert_eq!(sockets.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn successful_sse_headers_and_heartbeats_keep_idle_observation_socket_open() {
    let raw = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = raw.local_addr().unwrap();
    let mut listener = BoundedListener {
        inner: raw,
        sockets: Arc::new(Semaphore::new(1)),
    };
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut server, _) = axum::serve::Listener::accept(&mut listener).await;
    server
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
        .await
        .unwrap();
    // No client request bytes arrive during active idle SSE. Response heartbeats alone
    // must keep it alive beyond the initial five-second header-trickle allowance.
    for _ in 0..3 {
        tokio::time::advance(HEARTBEAT).await;
        server.write_all(b": heartbeat\n\n").await.unwrap();
    }
    let mut received = [0; 256];
    assert!(client.read(&mut received).await.unwrap() > 0);
}
