use alloy::{primitives::B256, sol_types::SolCall};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};

use crate::evm::{AxelarAmplifierGateway, AxelarServiceGovernance};

pub(super) struct Fixture {
    pub url: String,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Vec<Value>>,
}

impl Fixture {
    pub async fn finish(self) -> Vec<Value> {
        self.stop.send(()).unwrap();
        self.task.await.unwrap()
    }
}

pub(super) async fn serve(consumed: bool, pending: bool) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut state = ChainState {
            consumed,
            pending,
            next_hash: 1,
        };
        loop {
            let (mut stream, _) = tokio::select! {
                _ = &mut stopped => break,
                connection = listener.accept() => connection.unwrap(),
            };
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(stream.read_u8().await.unwrap());
            }
            let size = String::from_utf8_lossy(&headers)
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; size];
            stream.read_exact(&mut body).await.unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let body = json!({"jsonrpc":"2.0", "id":request["id"], "result":state.reply(&request)})
                .to_string();
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
            requests.push(request);
        }
        requests
    });
    Fixture { url, stop, task }
}

struct ChainState {
    consumed: bool,
    pending: bool,
    next_hash: u8,
}

impl ChainState {
    fn reply(&mut self, request: &Value) -> Value {
        match request["method"].as_str().unwrap() {
            "eth_call" => {
                let input = input(&request["params"][0]);
                let value = if input
                    .starts_with(&AxelarAmplifierGateway::isMessageExecutedCall::SELECTOR)
                {
                    self.consumed
                } else if input
                    .starts_with(&AxelarAmplifierGateway::isMessageApprovedCall::SELECTOR)
                {
                    !self.consumed
                } else if input
                    .starts_with(&AxelarServiceGovernance::isOperatorProposalApprovedCall::SELECTOR)
                    || input.starts_with(&AxelarServiceGovernance::getProposalEtaCall::SELECTOR)
                {
                    self.pending
                } else {
                    panic!("unexpected eth_call: {request}");
                };
                json!(format!("0x{:064x}", u8::from(value)))
            }
            "eth_getBalance" => json!("0x1000000000000000"),
            "eth_blockNumber" => json!("0x1"),
            "eth_sendTransaction" => {
                let input = input(&request["params"][0]);
                if input.starts_with(&AxelarServiceGovernance::executeCall::SELECTOR) {
                    assert!(!self.consumed, "must not consume the same message twice");
                    self.consumed = true;
                    self.pending = true;
                } else if input
                    .starts_with(&AxelarServiceGovernance::executeOperatorProposalCall::SELECTOR)
                    || input.starts_with(&AxelarServiceGovernance::executeProposalCall::SELECTOR)
                {
                    assert!(self.pending, "must have an approval or schedule");
                    self.pending = false;
                } else {
                    panic!("unexpected transaction: {request}");
                }
                let hash = B256::repeat_byte(self.next_hash);
                self.next_hash += 1;
                json!(hash)
            }
            "eth_getTransactionReceipt" => json!({
                "type":"0x0", "status":"0x1", "cumulativeGasUsed":"0x5208",
                "logs":[], "logsBloom":format!("0x{}", "00".repeat(256)),
                "transactionHash":request["params"][0], "transactionIndex":"0x0",
                "blockHash":B256::repeat_byte(9), "blockNumber":"0x1", "gasUsed":"0x5208",
                "effectiveGasPrice":"0x1", "from":format!("0x{}", "01".repeat(20)),
                "to":format!("0x{}", "02".repeat(20)), "contractAddress":null
            }),
            method => panic!("unexpected RPC method {method}: {request}"),
        }
    }
}

pub(super) fn input(transaction: &Value) -> Vec<u8> {
    alloy::hex::decode(
        transaction
            .get("input")
            .or_else(|| transaction.get("data"))
            .unwrap()
            .as_str()
            .unwrap(),
    )
    .unwrap()
}
