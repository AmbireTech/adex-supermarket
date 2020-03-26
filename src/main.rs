#![deny(clippy::all)]
#![deny(rust_2018_idioms)]
use std::net::SocketAddr;

use supermarket::serve;

#[tokio::main]
async fn main() {
    // Construct our SocketAddr to listen on...
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    serve(addr).await;
}
