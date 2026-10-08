use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use super::check_axelar_balance;

#[tokio::test]
async fn balance_checks_use_account_address_in_path_and_denom_in_query() {
    let address = "axelar1vykg4kxuanj87nsx7qllxuqxt2gk3g0lfgs29h";
    for (account_exists, balance, expected) in
        [(true, 200, true), (true, 50, false), (false, 0, false)]
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let lcd = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut responses = vec![(
                format!("/cosmos/auth/v1beta1/accounts/{address}"),
                if account_exists {
                    r#"{"account":{"account_number":"1","sequence":"0"}}"#.into()
                } else {
                    "{}".into()
                },
            )];
            if account_exists {
                responses.push((
                    format!("/cosmos/bank/v1beta1/balances/{address}/by_denom?denom=uaxl"),
                    serde_json::json!({"balance":{"denom":"uaxl","amount":balance.to_string()}})
                        .to_string(),
                ));
            }
            for (path, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = [0; 4096];
                let n = stream.read(&mut bytes).unwrap();
                assert!(
                    String::from_utf8_lossy(&bytes[..n])
                        .starts_with(&format!("GET {path} HTTP/1.1"))
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let result = check_axelar_balance(
            &lcd,
            "test-chain",
            &address.parse().unwrap(),
            &"uaxl".parse().unwrap(),
            100,
        )
        .await;
        assert_eq!(result.is_ok(), expected);
        if let Err(error) = result {
            assert!(error.to_string().contains(&format!("fund {address}")));
        }
        server.join().unwrap();
    }
    assert!("uaxl".parse::<cosmrs::AccountId>().is_err());
}
