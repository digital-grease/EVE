/// Minimal SIP User Agent (UAC/UAS) for point-to-point calls.
///
/// Implements only the happy-path flow needed by eve:
/// - UAC: INVITE → 200 OK → ACK → media → BYE → 200 OK
/// - UAS: wait for INVITE → 200 OK → wait for ACK → media → BYE → 200 OK
///
/// No registration, proxy, TLS, or re-INVITE support.
use crate::config::VoipConfig;
use crate::transport::udp::UdpTransport;
use std::net::SocketAddr;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

/// Default timeout for SIP responses (10 seconds).
const SIP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// SIP message helpers
// ---------------------------------------------------------------------------

/// Build a SIP INVITE message (UAC → UAS).
///
/// `call_id` is a unique identifier for the call session.
/// `local_addr` is the UAC's SIP socket address.
/// `rtp_port` is the UAC's RTP listening port.
pub fn build_invite(
    dest: SocketAddr,
    local_addr: SocketAddr,
    call_id: &str,
    rtp_port: u16,
) -> String {
    let sdp = build_sdp(local_addr.ip().to_string().as_str(), rtp_port);
    let sdp_len = sdp.len();
    format!(
        "INVITE sip:receiver@{dest} SIP/2.0\r\n\
Via: SIP/2.0/UDP {local_addr};rport\r\n\
From: <sip:sender@{local_addr}>;tag=eve-sender\r\n\
To: <sip:receiver@{dest}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {sdp_len}\r\n\
\r\n\
{sdp}"
    )
}

/// Build a SIP 200 OK response (UAS → UAC).
///
/// `from_header` should be the From header value echoed from the request.
pub fn build_200_ok(
    call_id: &str,
    local_addr: SocketAddr,
    rtp_port: u16,
    cseq_method: &str,
    from_header: &str,
) -> String {
    let sdp = build_sdp(local_addr.ip().to_string().as_str(), rtp_port);
    let sdp_len = sdp.len();
    format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP {local_addr};rport\r\n\
{from_header}\r\n\
To: <sip:receiver@{local_addr}>;tag=eve-receiver\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 {cseq_method}\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {sdp_len}\r\n\
\r\n\
{sdp}"
    )
}

/// Build a SIP ACK message (UAC → UAS).
pub fn build_ack(dest: SocketAddr, local_addr: SocketAddr, call_id: &str) -> String {
    format!(
        "ACK sip:receiver@{dest} SIP/2.0\r\n\
Via: SIP/2.0/UDP {local_addr};rport\r\n\
From: <sip:sender@{local_addr}>;tag=eve-sender\r\n\
To: <sip:receiver@{dest}>;tag=eve-receiver\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 ACK\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\
\r\n"
    )
}

/// Build a SIP BYE message (UAC → UAS).
pub fn build_bye(dest: SocketAddr, local_addr: SocketAddr, call_id: &str) -> String {
    format!(
        "BYE sip:receiver@{dest} SIP/2.0\r\n\
Via: SIP/2.0/UDP {local_addr};rport\r\n\
From: <sip:sender@{local_addr}>;tag=eve-sender\r\n\
To: <sip:receiver@{dest}>;tag=eve-receiver\r\n\
Call-ID: {call_id}\r\n\
CSeq: 2 BYE\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\
\r\n"
    )
}

/// Build a minimal SDP body offering PCMU at `ip`:`rtp_port`.
pub fn build_sdp(ip: &str, rtp_port: u16) -> String {
    format!(
        "v=0\r\n\
o=eve 1 1 IN IP4 {ip}\r\n\
s=eve\r\n\
c=IN IP4 {ip}\r\n\
t=0 0\r\n\
m=audio {rtp_port} RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n"
    )
}

// ---------------------------------------------------------------------------
// SIP message parsing
// ---------------------------------------------------------------------------

/// Parse the RTP port from an SDP body embedded in a SIP message.
///
/// Looks for `m=audio <port> RTP/AVP`.
pub fn parse_sdp_rtp_port(message: &str) -> Option<u16> {
    for line in message.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("m=audio ") {
            let port_str = rest.split_whitespace().next()?;
            return port_str.parse().ok();
        }
    }
    None
}

/// Parse the Call-ID header value from a SIP message (case-insensitive).
pub fn parse_call_id(message: &str) -> Option<String> {
    for line in message.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest_start) = lower.strip_prefix("call-id:") {
            // Use the original line (preserving case) to extract the value.
            let colon_pos = trimmed.find(':').unwrap() + 1;
            let value = trimmed[colon_pos..].trim();
            if !rest_start.is_empty() || !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Extract the From header line from a SIP message.
fn parse_from_header(message: &str) -> Option<String> {
    for line in message.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("from:") || lower.starts_with("from :") {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// Return `true` if `message` starts with a given SIP method or status code.
pub fn is_method(message: &str, method: &str) -> bool {
    message.starts_with(method)
}

// ---------------------------------------------------------------------------
// SIP agent
// ---------------------------------------------------------------------------

/// Minimal SIP agent supporting INVITE / 200 OK / ACK / BYE.
///
/// Binds a single SIP socket lazily and reuses it for all signalling
/// within the call session.
pub struct SipAgent {
    config: VoipConfig,
    transport: Option<UdpTransport>,
}

impl SipAgent {
    /// Create a new SIP agent with the given configuration.
    pub fn new(config: VoipConfig) -> Self {
        Self {
            config,
            transport: None,
        }
    }

    /// Ensure the SIP socket is bound.  After this call, `self.transport`
    /// is `Some`.
    async fn ensure_transport(&mut self) -> Result<(), super::VoipError> {
        if self.transport.is_none() {
            let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
            let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
            let t = UdpTransport::bind(local_addr)
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;
            self.transport = Some(t);
        }
        Ok(())
    }

    fn transport(&self) -> &UdpTransport {
        self.transport.as_ref().expect("transport not bound; call ensure_transport first")
    }

    /// Share the underlying UDP transport from another SipAgent.
    ///
    /// This allows a spawned task to reuse the same bound SIP socket
    /// without rebinding the port.
    pub fn share_transport_from(&mut self, other: &SipAgent) {
        if let Some(ref t) = other.transport {
            self.transport = Some(t.clone());
        }
    }

    /// (UAC) Send INVITE to `dest`, wait for 200 OK, send ACK.
    ///
    /// Returns the receiver's RTP port parsed from the 200 OK SDP.
    pub async fn invite(&mut self, dest: SocketAddr, call_id: &str) -> Result<u16, super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        self.ensure_transport().await?;
        let transport = self.transport();

        info!(%dest, call_id, "SIP INVITE →");
        let invite = build_invite(dest, local_addr, call_id, self.config.rtp_port);
        transport
            .send_to(invite.as_bytes(), dest)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;

        // Wait for 200 OK with timeout.
        let result = timeout(SIP_TIMEOUT, async {
            loop {
                let (data, _src) = transport
                    .recv_from()
                    .await
                    .map_err(|e| super::VoipError::Sip(e.to_string()))?;
                let msg = String::from_utf8_lossy(&data);
                if msg.starts_with("SIP/2.0 200") {
                    let rtp_port = parse_sdp_rtp_port(&msg)
                        .ok_or_else(|| {
                            warn!("200 OK missing RTP port in SDP");
                            super::VoipError::Sip("no RTP port in 200 OK".into())
                        })?;

                    info!(rtp_port, "200 OK received, sending ACK");
                    let ack = build_ack(dest, local_addr, call_id);
                    transport
                        .send_to(ack.as_bytes(), dest)
                        .await
                        .map_err(|e| super::VoipError::Sip(e.to_string()))?;

                    return Ok(rtp_port);
                }
            }
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(timeout_secs = SIP_TIMEOUT.as_secs(), "INVITE timed out waiting for 200 OK");
                Err(super::VoipError::Sip("INVITE timed out waiting for 200 OK".into()))
            }
        }
    }

    /// (UAC) Send BYE to `dest` to terminate the call.
    pub async fn bye(&mut self, dest: SocketAddr, call_id: &str) -> Result<(), super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        self.ensure_transport().await?;
        let transport = self.transport();

        info!(%dest, call_id, "SIP BYE →");
        let bye = build_bye(dest, local_addr, call_id);
        transport
            .send_to(bye.as_bytes(), dest)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;
        Ok(())
    }

    /// (UAS) Listen for INVITE, respond 200 OK with own RTP port, wait for ACK.
    ///
    /// Returns the caller's RTP address (IP + RTP port from their SDP).
    pub async fn accept(&mut self) -> Result<(SocketAddr, String), super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        self.ensure_transport().await?;
        let transport = self.transport();

        info!(sip_port = self.config.sip_port, "listening for INVITE");

        // Wait for INVITE.
        loop {
            let (data, src) = transport
                .recv_from()
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;
            let msg = String::from_utf8_lossy(&data);
            if !is_method(&msg, "INVITE") {
                debug!(%src, "ignored non-INVITE message");
                continue;
            }

            let call_id = parse_call_id(&msg)
                .ok_or_else(|| {
                    warn!(%src, "INVITE missing Call-ID");
                    super::VoipError::Sip("INVITE missing Call-ID".into())
                })?;
            let caller_rtp_port = parse_sdp_rtp_port(&msg)
                .ok_or_else(|| {
                    warn!(%src, call_id = %call_id, "INVITE missing RTP port");
                    super::VoipError::Sip("INVITE missing RTP port".into())
                })?;

            info!(%src, call_id = %call_id, caller_rtp_port, "INVITE received");

            // Echo the From header from the INVITE into the 200 OK.
            let from_header = parse_from_header(&msg)
                .unwrap_or_else(|| format!("From: <sip:sender@{src}>;tag=eve-sender"));

            // Respond 200 OK.
            let ok = build_200_ok(&call_id, local_addr, self.config.rtp_port, "INVITE", &from_header);
            transport
                .send_to(ok.as_bytes(), src)
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;
            debug!("200 OK sent");

            // Wait for ACK with Call-ID verification and timeout.
            let ack_result = timeout(SIP_TIMEOUT, async {
                loop {
                    let (ack_data, _) = transport
                        .recv_from()
                        .await
                        .map_err(|e| super::VoipError::Sip(e.to_string()))?;
                    let ack_msg = String::from_utf8_lossy(&ack_data);
                    if is_method(&ack_msg, "ACK") {
                        // Verify Call-ID matches.
                        if let Some(ack_cid) = parse_call_id(&ack_msg) {
                            if ack_cid != call_id {
                                debug!(ack_call_id = %ack_cid, "ignoring ACK for different call");
                                continue; // ACK for a different call
                            }
                        }
                        let caller_rtp_addr = SocketAddr::new(src.ip(), caller_rtp_port);
                        info!(%caller_rtp_addr, "ACK received, call established");
                        return Ok::<_, super::VoipError>((caller_rtp_addr, call_id.clone()));
                    }
                }
            })
            .await;

            return match ack_result {
                Ok(inner) => inner,
                Err(_) => {
                    warn!(timeout_secs = SIP_TIMEOUT.as_secs(), "timed out waiting for ACK");
                    Err(super::VoipError::Sip("timed out waiting for ACK".into()))
                }
            };
        }
    }

    /// (UAS) Wait for BYE and respond 200 OK.
    pub async fn wait_for_bye(&self, call_id: &str) -> Result<(), super::VoipError> {
        // wait_for_bye is called from a spawned task that may not have access
        // to the shared transport. Bind a fresh socket with SO_REUSEADDR semantics
        // or, if the main transport is still alive, use it.  For simplicity we
        // bind a new socket on port 0 (ephemeral) and have the BYE arrive at the
        // well-known SIP port; alternatively we re-bind on the same port.
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        let transport = if let Some(ref t) = self.transport {
            t.clone()
        } else {
            UdpTransport::bind(local_addr)
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?
        };

        debug!(call_id, "waiting for BYE");
        let result = timeout(Duration::from_secs(120), async {
            loop {
                let (data, src) = transport
                    .recv_from()
                    .await
                    .map_err(|e| super::VoipError::Sip(e.to_string()))?;
                let msg = String::from_utf8_lossy(&data);
                if is_method(&msg, "BYE") {
                    if let Some(cid) = parse_call_id(&msg) {
                        if cid != call_id {
                            debug!(bye_call_id = %cid, "ignoring BYE for different call");
                            continue; // different session
                        }
                    }
                    info!(call_id, "BYE received, sending 200 OK");
                    let ok =
                        format!("SIP/2.0 200 OK\r\nCall-ID: {call_id}\r\nContent-Length: 0\r\n\r\n");
                    transport
                        .send_to(ok.as_bytes(), src)
                        .await
                        .map_err(|e| super::VoipError::Sip(e.to_string()))?;
                    return Ok(());
                }
            }
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => {
                warn!(call_id, "timed out (120s) waiting for BYE");
                Err(super::VoipError::Sip("timed out waiting for BYE".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sdp_contains_port() {
        let sdp = build_sdp("127.0.0.1", 10000);
        assert!(sdp.contains("m=audio 10000 RTP/AVP 0"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
    }

    #[test]
    fn test_parse_sdp_rtp_port() {
        let sdp = build_sdp("127.0.0.1", 12345);
        let invite = format!("INVITE sip:x SIP/2.0\r\n\r\n{sdp}");
        assert_eq!(parse_sdp_rtp_port(&invite), Some(12345));
    }

    #[test]
    fn test_parse_sdp_rtp_port_missing() {
        assert_eq!(parse_sdp_rtp_port("no sdp here"), None);
    }

    #[test]
    fn test_parse_call_id() {
        let msg = "INVITE sip:x SIP/2.0\r\nCall-ID: abc123\r\n\r\n";
        assert_eq!(parse_call_id(msg), Some("abc123".into()));
    }

    #[test]
    fn test_parse_call_id_case_insensitive() {
        let msg = "INVITE sip:x SIP/2.0\r\ncall-id: abc123\r\n\r\n";
        assert_eq!(parse_call_id(msg), Some("abc123".into()));
        let msg2 = "INVITE sip:x SIP/2.0\r\nCall-Id: abc123\r\n\r\n";
        assert_eq!(parse_call_id(msg2), Some("abc123".into()));
    }

    #[test]
    fn test_is_method() {
        assert!(is_method("INVITE sip:x SIP/2.0", "INVITE"));
        assert!(is_method("SIP/2.0 200 OK", "SIP/2.0 200"));
        assert!(!is_method("ACK sip:x", "INVITE"));
    }

    #[test]
    fn test_build_invite_contains_sdp() {
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5061".parse().unwrap();
        let invite = build_invite(dest, local, "call-abc", 10000);
        assert!(invite.starts_with("INVITE sip:receiver@127.0.0.1:5060 SIP/2.0"));
        assert!(invite.contains("Call-ID: call-abc"));
        assert!(invite.contains("m=audio 10000"));
        assert!(invite.contains("Max-Forwards: 70"));
    }

    #[test]
    fn test_build_invite_no_leading_whitespace() {
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5061".parse().unwrap();
        let invite = build_invite(dest, local, "call-abc", 10000);
        for line in invite.lines() {
            assert!(
                !line.starts_with(' '),
                "SIP line has leading whitespace: {line:?}"
            );
        }
    }

    #[test]
    fn test_build_sdp_no_leading_whitespace() {
        let sdp = build_sdp("127.0.0.1", 10000);
        for line in sdp.lines() {
            assert!(
                !line.starts_with(' '),
                "SDP line has leading whitespace: {line:?}"
            );
        }
    }

    #[test]
    fn test_build_ack() {
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5061".parse().unwrap();
        let ack = build_ack(dest, local, "call-abc");
        assert!(ack.starts_with("ACK"));
        assert!(ack.contains("CSeq: 1 ACK"));
    }

    #[test]
    fn test_build_bye() {
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5061".parse().unwrap();
        let bye = build_bye(dest, local, "call-abc");
        assert!(bye.starts_with("BYE"));
        assert!(bye.contains("CSeq: 2 BYE"));
    }

    #[test]
    fn test_build_200_ok_echoes_from_header() {
        let local: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let from = "From: <sip:sender@10.0.0.1:5061>;tag=eve-sender";
        let ok = build_200_ok("call-abc", local, 10000, "INVITE", from);
        assert!(ok.contains(from), "200 OK should echo the From header");
    }
}
