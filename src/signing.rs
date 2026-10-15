//! Webhook signatures: `Webhook-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "{t}.{raw body}")>`.
//! Signing the timestamp together with the body lets receivers reject replays of old deliveries.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Receivers should reject signatures older (or newer) than this.
pub const TOLERANCE_SECS: i64 = 300;

fn mac(secret: &str, timestamp: i64, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

pub fn signature_header(secret: &str, timestamp: i64, body: &[u8]) -> String {
    format!("t={timestamp},v1={}", hex::encode(mac(secret, timestamp, body).finalize().into_bytes()))
}

/// What a receiver does: parse, check freshness, recompute and compare in constant time.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> Result<(), &'static str> {
    let mut timestamp = None;
    let mut signature = None;
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => timestamp = v.parse::<i64>().ok(),
            Some(("v1", v)) => signature = hex::decode(v).ok(),
            _ => {}
        }
    }
    let (timestamp, signature) = timestamp.zip(signature).ok_or("malformed signature header")?;
    if (now - timestamp).abs() > TOLERANCE_SECS {
        return Err("timestamp outside tolerance");
    }
    mac(secret, timestamp, body).verify_slice(&signature).map_err(|_| "signature mismatch")
}
