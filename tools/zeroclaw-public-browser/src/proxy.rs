use crate::policy::{select_public_address, validate_host};
use anyhow::{Result, bail};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

pub fn connect_host(header: &str) -> Result<String> {
    let mut words = header.lines().next().unwrap_or("").split_whitespace();
    if words.next() != Some("CONNECT") {
        bail!("Only HTTPS tunneling is permitted.");
    }
    let authority = words
        .next()
        .ok_or_else(|| anyhow::Error::msg("Missing authority"))?;
    let host = authority
        .strip_suffix(":443")
        .ok_or_else(|| anyhow::Error::msg("Only port 443 is permitted"))?;
    if !matches!(words.next(), Some("HTTP/1.1" | "HTTP/1.0")) || words.next().is_some() {
        bail!("Invalid proxy request");
    }
    validate_host(host)
}

async fn serve_one(mut client: TcpStream) -> Result<()> {
    let mut header = Vec::new();
    // Bound proxy request time and memory, and consume no TLS bytes early.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !header.ends_with(b"\r\n\r\n") {
            if header.len() >= 16_384 {
                bail!("Proxy headers too large");
            }
            header.push(client.read_u8().await?);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let destination = async {
        let host = connect_host(std::str::from_utf8(&header)?)?;
        let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), 443))
            .await?
            .collect();
        let address = select_public_address(&addresses)?;
        Ok::<_, anyhow::Error>(TcpStream::connect(address).await?)
    };
    let mut upstream = match tokio::time::timeout(Duration::from_secs(10), destination).await {
        Ok(Ok(stream)) => stream,
        _ => {
            client
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            return Ok(());
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    // TLS remains end-to-end with normal browser certificate validation.
    tokio::time::timeout(
        Duration::from_secs(300),
        tokio::io::copy_bidirectional(&mut client, &mut upstream),
    )
    .await??;
    Ok(())
}

pub async fn serve(listener: TcpListener) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept(), if connections.len() < 128 => {
                let (stream, _) = result?;
                connections.spawn(serve_one(stream));
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_checks_each_destination() {
        assert_eq!(
            connect_host("CONNECT example.com:443 HTTP/1.1\r\n").unwrap(),
            "example.com"
        );
        for h in [
            "CONNECT www.linkedin.com:443 HTTP/1.1",
            "CONNECT lnkd.in:443 HTTP/1.1",
            "CONNECT 127.0.0.1:443 HTTP/1.1",
            "CONNECT example.com:80 HTTP/1.1",
            "GET https://example.com HTTP/1.1",
            "CONNECT example.com:443 HTTP/1.1 extra",
        ] {
            assert!(connect_host(h).is_err(), "{h}");
        }
    }
    #[tokio::test]
    async fn actual_proxy_rejects_linkedin_and_local_targets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = zeroclaw_spawn::spawn!(serve(listener));
        for host in [
            "www.linkedin.com",
            "api.linkedin.com",
            "lnkd.in",
            "127.0.0.1",
            "foo.local",
        ] {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(
                    format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 403"), "{host}");
        }
        task.abort();
    }
}
