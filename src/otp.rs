//! One-time codes for two-factor sign-ins and enrollments.
//!
//! Two halves, deliberately small and dependency-light:
//!
//! - `parse` reads an `otpauth://totp/...` URI (what a setup QR encodes) or a
//!   bare base32 seed, keeping the algorithm, digit count and period the
//!   issuer asked for instead of silently assuming the defaults.
//! - `code_at` is RFC 6238 TOTP over HMAC-SHA1/256/512. The crates already
//!   in the tree (`hmac`, `sha2`, plus `sha1` for the ubiquitous default)
//!   do the cryptography; there is no OTP library and no network anything.
//!
//! A parsed seed is a persistent secret: it belongs sealed in the vault,
//! never in a log line, a chat message or a page. Nothing here prints one.

use hmac::{Hmac, Mac};

/// Hash the issuer picked for its codes. SHA-1 is the de-facto default;
/// SHA-256/512 show up in `otpauth` URIs and some authenticators.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    fn from_name(name: &str) -> Result<Self, String> {
        match name.to_ascii_uppercase().as_str() {
            "" | "SHA1" => Ok(Self::Sha1),
            "SHA256" => Ok(Self::Sha256),
            "SHA512" => Ok(Self::Sha512),
            other => Err(format!("unsupported OTP algorithm {other}")),
        }
    }
}

/// A parsed `otpauth://` URI (or bare seed), ready to mint codes.
pub struct Totp {
    pub secret: Vec<u8>,
    pub algorithm: Algorithm,
    pub digits: u32,
    pub period: u64,
    /// Issuer and account, for labelling only — never needed to mint a code.
    pub issuer: String,
    pub account: String,
}

impl Totp {
    /// The current code and the seconds left in its time step.
    pub fn code(&self) -> (String, u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (self.code_at(now), self.period - (now % self.period))
    }

    /// The code for the step containing `time` (seconds since the epoch).
    pub fn code_at(&self, time: u64) -> String {
        let counter = time / self.period;
        let mac = match self.algorithm {
            Algorithm::Sha1 => {
                let mut m = <Hmac<sha1::Sha1> as Mac>::new_from_slice(&self.secret)
                    .expect("HMAC takes any key length");
                m.update(&counter.to_be_bytes());
                m.finalize().into_bytes().to_vec()
            }
            Algorithm::Sha256 => {
                let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(&self.secret)
                    .expect("HMAC takes any key length");
                m.update(&counter.to_be_bytes());
                m.finalize().into_bytes().to_vec()
            }
            Algorithm::Sha512 => {
                let mut m = <Hmac<sha2::Sha512> as Mac>::new_from_slice(&self.secret)
                    .expect("HMAC takes any key length");
                m.update(&counter.to_be_bytes());
                m.finalize().into_bytes().to_vec()
            }
        };
        // RFC 4226 dynamic truncation.
        let offset = (mac[mac.len() - 1] & 0x0f) as usize;
        let binary = ((u32::from(mac[offset]) & 0x7f) << 24)
            | (u32::from(mac[offset + 1]) << 16)
            | (u32::from(mac[offset + 2]) << 8)
            | u32::from(mac[offset + 3]);
        let modulus = 10u32.pow(self.digits);
        format!("{:0width$}", binary % modulus, width = self.digits as usize)
    }
}

/// Parse an `otpauth://totp/...` URI or a bare base32 seed. HOTP, push and
/// proprietary tokens are different methods and are refused, not guessed at.
pub fn parse(input: &str) -> Result<Totp, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty OTP setup".into());
    }
    let Some(rest) = input.strip_prefix("otpauth://") else {
        // A bare "can't scan?" manual key: the site's defaults are SHA-1,
        // six digits, thirty seconds.
        return Ok(Totp {
            secret: base32_decode(input)?,
            algorithm: Algorithm::Sha1,
            digits: 6,
            period: 30,
            issuer: String::new(),
            account: String::new(),
        });
    };
    let (kind, rest) = rest
        .split_once('/')
        .ok_or("malformed otpauth URI (no type)")?;
    if !kind.eq_ignore_ascii_case("totp") {
        return Err(format!(
            "otpauth type {kind} is not TOTP; only TOTP can mint codes"
        ));
    }
    let (label, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut secret = None;
    let mut algorithm = Algorithm::Sha1;
    let mut digits = 6u32;
    let mut period = 30u64;
    let mut issuer = String::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key.to_ascii_lowercase().as_str() {
            "secret" => secret = Some(base32_decode(&url_decode(value))?),
            "algorithm" => algorithm = Algorithm::from_name(&url_decode(value))?,
            "digits" => {
                digits = url_decode(value)
                    .parse()
                    .map_err(|_| "bad digits in otpauth URI".to_string())?
            }
            "period" => {
                period = url_decode(value)
                    .parse()
                    .map_err(|_| "bad period in otpauth URI".to_string())?
            }
            "issuer" => issuer = url_decode(value),
            _ => {}
        }
    }
    let Some(secret) = secret else {
        return Err("otpauth URI without a secret".into());
    };
    if !(4..=8).contains(&digits) {
        return Err(format!("implausible digit count {digits}"));
    }
    if !(5..=300).contains(&period) {
        return Err(format!("implausible period {period}s"));
    }
    // The label is "issuer:account" or just "account"; a ?issuer= wins.
    let label = url_decode(label);
    let (label_issuer, account) = match label.split_once(':') {
        Some((i, a)) => (i.trim().to_string(), a.trim().to_string()),
        None => (String::new(), label.trim().to_string()),
    };
    if issuer.is_empty() {
        issuer = label_issuer;
    }
    Ok(Totp {
        secret,
        algorithm,
        digits,
        period,
        issuer,
        account,
    })
}

/// RFC 4648 base32 (the alphabet authenticator seeds use), tolerant of lower
/// case, spaces and missing padding — manual keys arrive formatted.
fn base32_decode(input: &str) -> Result<Vec<u8>, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in input
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        let upper = c.to_ascii_uppercase();
        let value = ALPHABET
            .iter()
            .position(|&a| a == upper)
            .ok_or("seed is not base32")? as u32;
        buf = (buf << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    if out.is_empty() {
        return Err("empty seed".into());
    }
    Ok(out)
}

/// Percent-decoding for the `otpauth` query (`%20`, `%3A`, ...).
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

