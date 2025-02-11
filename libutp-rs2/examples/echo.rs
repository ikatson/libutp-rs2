use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context};
use futures::FutureExt;

use libutp_rs2::{UtpUdpContext, UtpUdpStream};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
    try_join,
};
use tracing::{error_span, info, Instrument};

const MAX_COUNTER: u64 = 1_000_000;
const TIMEOUT: Duration = Duration::from_secs(2);
const PRINT_EVERY: u64 = 100_000;

async fn flatten<JoinError>(
    handle: impl Future<Output = Result<anyhow::Result<()>, JoinError>>,
) -> anyhow::Result<()> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(_) => bail!("joining failed"),
    }
}

async fn echo(stream: UtpUdpStream) -> anyhow::Result<()> {
    let (reader, writer) = tokio::io::split(stream);

    let mut reader = reader;

    let reader = async move {
        for expected in 0..=MAX_COUNTER {
            let current = timeout(TIMEOUT, reader.read_u64())
                .await
                .context("timeout reading")?
                .context("error reading")?;
            if current != expected {
                bail!("expected {expected}, got {current}");
            }

            if current % PRINT_EVERY == 0 {
                info!("current counter {current}");
            }
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    let writer = async move {
        let mut writer = writer;
        for counter in 0..=MAX_COUNTER {
            timeout(TIMEOUT, writer.write_u64(counter))
                .await
                .context("timeout writing")?
                .context("error writing")?;
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    try_join!(reader, writer)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let client_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 8001).into();
    let server_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 8002).into();

    let listener = UtpUdpContext::new_udp(server_addr)
        .await
        .context("error creating socket")?;

    let client = tokio::spawn(
        async move {
            let client = UtpUdpContext::new_udp(client_addr)
                .await
                .context("error creating socket")?;
            let sock = timeout(TIMEOUT, client.connect(server_addr))
                .await
                .context("timeout connecting")?
                .context("error connecting")?;
            echo(sock).await.context("error running client echo")
        }
        .instrument(error_span!("client"))
        .map(|v| v.context("client died")),
    )
    .map(|v| v.context("error joining"));

    let server = tokio::spawn(
        async move {
            let sock = listener.accept().await.context("error accepting")?;
            echo(sock).await.context("error running server echo")
        }
        .instrument(error_span!("server"))
        .map(|v| v.context("server died")),
    );

    try_join!(flatten(client), flatten(server))?;
    info!("finished");

    Ok(())
}
