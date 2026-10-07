#!/usr/bin/env python3
"""Exercise initial gateway signer deployment against a fresh, local Anvil process.

Requires anvil and cast on PATH, plus the deployment checkout's installed
@axelar-network/axelar-gmp-sdk-solidity artifacts. No external RPC or keys.
"""
import argparse
import json
from pathlib import Path
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path, help="SDK artifacts/contracts directory")
    args = parser.parse_args()
    cast_bin, anvil_bin = shutil.which("cast"), shutil.which("anvil")
    if not cast_bin or not anvil_bin:
        parser.error("cast and anvil must be on PATH")
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    node = subprocess.Popen([anvil_bin, "--host", "127.0.0.1", "--port", str(port), "--silent"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        exercise(args.artifacts, cast_bin, port)
    finally:
        node.terminate()
        node.wait(timeout=10)


def exercise(root, cast_bin, port):
    def rpc(method, params):
        data = json.dumps(dict(jsonrpc="2.0", id=1, method=method, params=params)).encode()
        request = urllib.request.Request(f"http://127.0.0.1:{port}", data=data, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            result = json.load(response)
        if "error" in result:
            raise RuntimeError(result["error"])
        return result["result"]

    def cast(*args):
        return subprocess.check_output([cast_bin, *args], text=True).strip()

    for attempt in range(100):
        try:
            accounts = rpc("eth_accounts", [])
            break
        except urllib.error.URLError:
            if attempt == 99:
                raise
            time.sleep(0.05)
    owner = accounts[0]

    def send(data, to=None, sender=owner):
        transaction = {"from": sender, "data": data, "gas": "0x989680"}
        if to:
            transaction["to"] = to
        tx_hash = rpc("eth_sendTransaction", [transaction])
        for _ in range(100):
            receipt = rpc("eth_getTransactionReceipt", [tx_hash])
            if receipt:
                return receipt
            time.sleep(0.05)
        raise AssertionError("local transaction did not mine")

    def artifact(name):
        return json.loads((root / name).read_text())["bytecode"]

    def deploy(name, params):
        receipt = send(artifact(name) + params[2:])
        assert receipt["status"] == "0x1", receipt
        return receipt["contractAddress"]

    zero = "0x" + "00" * 32
    implementation = deploy("gateway/AxelarAmplifierGateway.sol/AxelarAmplifierGateway.json", cast("abi-encode", "f(uint256,bytes32,uint256)", "15", zero, "3600"))
    signers = ",".join(f"({address},1)" for address in sorted(accounts[1:4], key=lambda a: int(a, 16)))
    signer_set = f"([{signers}],2,{zero})"
    params = cast("abi-encode", "f(address,((address,uint128)[],uint128,bytes32)[])", owner, f"[{signer_set}]")
    proxy = deploy("gateway/AxelarAmplifierGatewayProxy.sol/AxelarAmplifierGatewayProxy.json", cast("abi-encode", "f(address,address,bytes)", implementation, owner, params))

    def call(signature, *args):
        return rpc("eth_call", [{"to": proxy, "data": cast("calldata", signature, *args)}, "latest"])

    assert int(call("epoch()"), 16) == 1
    expected = cast("keccak", cast("abi-encode", "f(((address,uint128)[],uint128,bytes32))", signer_set))
    assert call("signersHashByEpoch(uint256)", "1") == expected
    assert int(call("owner()"), 16) == int(owner, 16)
    assert int(call("operator()"), 16) == int(owner, 16)
    assert int(rpc("eth_getTransactionCount", [owner, "latest"]), 16) == 2
    print("PASS: original two gateway deployments initialize the exact 2-of-3 signer set, owner and operator; no pause or upgrade writes")


if __name__ == "__main__":
    main()
