// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use std::thread::sleep;
use first_transaction::{Account, FaucetClient, RestClient, FAUCET_URL, TESTNET_URL};
use std::thread;
use std::time::Duration;

//:!:>section_7
#[tokio::main]
async fn main() -> () {
    for i in 0..1 {
        thread::spawn(|| {
                transfer();
        });
    }
    sleep(Duration::from_secs(10));
    // println!("Bob: {:?}", rest_client.account_balance(&bob.address()));
    //
    // // Have Alice give Bob 10 coins
    // let tx_hash = rest_client.transfer(&mut alice, &bob.address(), 1_000);
    // rest_client.wait_for_transaction(&tx_hash);
    //
    // println!("\n=== Final Balances ===");
    // println!("Alice: {:?}", rest_client.account_balance(&alice.address()));
    // println!("Bob: {:?}", rest_client.account_balance(&bob.address()));
}
//<:!:section_7

fn transfer() {
    let rest_client = RestClient::new(TESTNET_URL.to_string());
    let faucet_client = FaucetClient::new(FAUCET_URL.to_string(), rest_client.clone());

    // Create two accounts, Alice and Bob, and fund Alice but not Bob


    let decoded = hex::decode("10E316BF45744B15F9FAF3834A9B8E748C459A3A4554539BBA4A9621AB71BF52").expect("Decoding failed");
    let mut alice = Account::new(Some(decoded));
    let decodedbob = hex::decode("f41548d059970000ac2636554148450f1b7db15516ed07cde4013a56a9d45b12").expect("Decoding failed");
    let bob = Account::new(Some(decodedbob));

    println!("\n=== Addresses ===");
    println!("Alice: 0x{}", alice.address());
    println!("Bob: 0x{}", bob.address());

    // faucet_client.fund_account(&alice.auth_key().as_str(), 1_000_000_000);
    // faucet_client.fund_account(&bob.auth_key().as_str(), 0);
    let tx_hash = rest_client.transfer(&mut alice, &bob.address(), 1_000);
    rest_client.wait_for_transaction(&tx_hash);

    println!("\n=== Initial Balances ===");
    println!("Alice: {:?}", rest_client.account_balance(&alice.address()));
    println!("Bob: {:?}", rest_client.account_balance(&bob.address()));
}