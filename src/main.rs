mod lib;
use crate::lib::{future, Async};
use std::net::TcpListener;

fn main() {
    println!("Hello! World!");

    let res = future::block_on(async {
        println!("block on: started");
        let listener = Async::<TcpListener>::bind(([127, 0, 0, 1], 8000))?;
        let (_, _) = listener.accept().await?;

        println!("Accepted client: {}", "addr");

        std::io::Result::Ok(())
    });

    assert!(res.is_ok());
}
