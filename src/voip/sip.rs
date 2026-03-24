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
    format!(
        "INVITE sip:receiver@{dest} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {local_addr};rport\r\n\
         From: <sip:sender@{local_addr}>;tag=eve-sender\r\n\
         To: <sip:receiver@{dest}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {sdp_len}\r\n\
         \r\n\
         {sdp}",
        sdp_len = sdp.len()
    )
}

/// Build a SIP 200 OK response (UAS → UAC).
pub fn build_200_ok(
    call_id: &str,
    local_addr: SocketAddr,
    rtp_port: u16,
    cseq_method: &str,
) -> String {
    let sdp = build_sdp(local_addr.ip().to_string().as_str(), rtp_port);
    format!(
        "SIP/2.0 200 OK\r\n\
         Via: SIP/2.0/UDP {local_addr};rport\r\n\
         From: <sip:sender@{local_addr}>;tag=eve-sender\r\n\
         To: <sip:receiver@{local_addr}>;tag=eve-receiver\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {cseq_method}\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {sdp_len}\r\n\
         \r\n\
         {sdp}",
        sdp_len = sdp.len()
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
        if let Some(rest) = line.strip_prefix("m=audio ") {
            let port_str = rest.split_whitespace().next()?;
            return port_str.parse().ok();
        }
    }
    None
}

/// Parse the Call-ID header value from a SIP message.
pub fn parse_call_id(message: &str) -> Option<String> {
    for line in message.lines() {
        if let Some(rest) = line.strip_prefix("Call-ID: ") {
            return Some(rest.trim().to_owned());
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
pub struct SipAgent {
    config: VoipConfig,
}

impl SipAgent {
    /// Create a new SIP agent with the given configuration.
    pub fn new(config: VoipConfig) -> Self {
        Self { config }
    }

    /// (UAC) Send INVITE to `dest`, wait for 200 OK, send ACK.
    ///
    /// Returns the receiver's RTP port parsed from the 200 OK SDP.
    pub async fn invite(&self, dest: SocketAddr, call_id: &str) -> Result<u16, super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        let transport = UdpTransport::bind(local_addr)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;

        let invite = build_invite(dest, local_addr, call_id, self.config.rtp_port);
        transport
            .send_to(invite.as_bytes(), dest)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;

        // Wait for 200 OK.
        loop {
            let (data, _src) = transport
                .recv_from()
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;
            let msg = String::from_utf8_lossy(&data);
            if msg.starts_with("SIP/2.0 200") {
                let rtp_port = parse_sdp_rtp_port(&msg)
                    .ok_or_else(|| super::VoipError::Sip("no RTP port in 200 OK".into()))?;

                // Send ACK.
                let ack = build_ack(dest, local_addr, call_id);
                transport
                    .send_to(ack.as_bytes(), dest)
                    .await
                    .map_err(|e| super::VoipError::Sip(e.to_string()))?;

                return Ok(rtp_port);
            }
        }
    }

    /// (UAC) Send BYE to `dest` to terminate the call.
    pub async fn bye(&self, dest: SocketAddr, call_id: &str) -> Result<(), super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        let transport = UdpTransport::bind(local_addr)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;

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
    pub async fn accept(&self) -> Result<(SocketAddr, String), super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        let transport = UdpTransport::bind(local_addr)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;

        // Wait for INVITE.
        loop {
            let (data, src) = transport
                .recv_from()
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;
            let msg = String::from_utf8_lossy(&data);
            if !is_method(&msg, "INVITE") {
                continue;
            }

            let call_id = parse_call_id(&msg)
                .ok_or_else(|| super::VoipError::Sip("INVITE missing Call-ID".into()))?;
            let caller_rtp_port = parse_sdp_rtp_port(&msg)
                .ok_or_else(|| super::VoipError::Sip("INVITE missing RTP port".into()))?;

            // Respond 200 OK.
            let ok = build_200_ok(&call_id, local_addr, self.config.rtp_port, "INVITE");
            transport
                .send_to(ok.as_bytes(), src)
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;

            // Wait for ACK.
            loop {
                let (ack_data, _) = transport
                    .recv_from()
                    .await
                    .map_err(|e| super::VoipError::Sip(e.to_string()))?;
                let ack_msg = String::from_utf8_lossy(&ack_data);
                if is_method(&ack_msg, "ACK") {
                    let caller_rtp_addr = SocketAddr::new(src.ip(), caller_rtp_port);
                    return Ok((caller_rtp_addr, call_id));
                }
            }
        }
    }

    /// (UAS) Wait for BYE and respond 200 OK.
    pub async fn wait_for_bye(&self, call_id: &str) -> Result<(), super::VoipError> {
        let local_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let local_addr = SocketAddr::new(local_ip, self.config.sip_port);
        let transport = UdpTransport::bind(local_addr)
            .await
            .map_err(|e| super::VoipError::Sip(e.to_string()))?;

        loop {
            let (data, src) = transport
                .recv_from()
                .await
                .map_err(|e| super::VoipError::Sip(e.to_string()))?;
            let msg = String::from_utf8_lossy(&data);
            if is_method(&msg, "BYE") {
                if let Some(cid) = parse_call_id(&msg) {
                    if cid != call_id {
                        continue; // different session
                    }
                }
                let ok =
                    format!("SIP/2.0 200 OK\r\nCall-ID: {call_id}\r\nContent-Length: 0\r\n\r\n");
                transport
                    .send_to(ok.as_bytes(), src)
                    .await
                    .map_err(|e| super::VoipError::Sip(e.to_string()))?;
                return Ok(());
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
}
