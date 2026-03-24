/// UDP socket wrapper for SIP and RTP.
/// Wraps a `tokio::net::UdpSocket` with send/recv helpers.
pub struct UdpTransport {
    socket: tokio::net::UdpSocket,
}

impl UdpTransport {
    /// Bind to the given local address.
    pub async fn bind(addr: std::net::SocketAddr) -> Result<Self, super::TransportError> {
        let socket = tokio::net::UdpSocket::bind(addr).await.map_err(|e| {
            super::TransportError::BindFailed {
                port: addr.port(),
                source: e,
            }
        })?;
        Ok(Self { socket })
    }

    /// Send `data` to `dest`.
    pub async fn send_to(
        &self,
        data: &[u8],
        dest: std::net::SocketAddr,
    ) -> Result<(), super::TransportError> {
        self.socket.send_to(data, dest).await?;
        Ok(())
    }

    /// Receive the next datagram. Returns (bytes, source address).
    pub async fn recv_from(
        &self,
    ) -> Result<(Vec<u8>, std::net::SocketAddr), super::TransportError> {
        let mut buf = vec![0u8; 65535];
        let (n, addr) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(n);
        Ok((buf, addr))
    }
}
