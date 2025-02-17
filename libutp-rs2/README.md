Async Tokio library for uTP (uTorrent transport protocol) using C-based [libutp](https://github.com/bittorrent/libutp) under the hood.

Working and tested alternative to [libutp-rs](https://crates.io/crates/libutp-rs)

[Docs](https://docs.rs/librqbit-rs2)

## Example

```rust

use libutp_rs2::UtpUdpContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Server
    let server_addr = "127.0.0.1:8001".parse()?;
    let listener = tokio::spawn(async move {
        let mut buf = String::new();
        UtpUdpContext::new_udp(server_addr)
            .await?
            .accept()
            .await?
            .read_to_string(&mut buf)
            .await?;
        Ok::<_, anyhow::Error>(buf)
    });

    // Client
    tokio::spawn(async move {
        UtpUdpContext::new_udp("127.0.0.1:8002".parse()?)
            .await?
            .connect(server_addr)
            .await?
            .write_all(b"hello")
            .await?;
        Ok::<_, anyhow::Error>(())
    });

    println!("{}", listener.await.unwrap().unwrap());

    Ok(())
}
```
