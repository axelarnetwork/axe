use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// None drops the connection after reading the request to simulate lost replies.
pub(super) async fn serve(responses: Vec<Option<Value>>) -> (String, JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let byte = stream.read_u8().await.unwrap();
                bytes.push(byte);
                if bytes.ends_with(b"\r\n\r\n") {
                    break bytes.len();
                }
            };
            let headers = String::from_utf8_lossy(&bytes);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            bytes.resize(header_end + length, 0);
            stream.read_exact(&mut bytes[header_end..]).await.unwrap();
            let request: Value = serde_json::from_slice(&bytes[header_end..]).unwrap();
            if let Some(mut response) = response {
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                let body = response.to_string();
                let wire = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(wire.as_bytes()).await.unwrap();
            }
            requests.push(request);
        }
        requests
    });
    (url, task)
}
