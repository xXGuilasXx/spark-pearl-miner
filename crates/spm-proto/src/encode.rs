//! Proof encoders and the per-pool proof-field learner.
//!
//! The wire value is always standard base64 (with padding) of:
//! - `plain`: bincode(PlainProof);
//! - `zstd`:  zstd(bincode, level 3) — the `plain_proof_zst` field used for HeroMiners by other miners;
//! - `gzip`:  gzip(bincode) — Kryptex "v2" sessions.
use std::io::{Read, Write};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::Dialect;

/// zstd level for `plain_proof_zst`.
pub const ZSTD_LEVEL: i32 = 3;
/// Decoding never inflates beyond this (a real proof is < 1 MiB); protects the mock pool and tools.
pub const MAX_DECODED_PROOF: usize = 16 * 1024 * 1024;
/// Consecutive format rejects on one field before the learner switches to the other field.
pub const MAX_FORMAT_REJECTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProofEncoding {
    Plain,
    Zstd,
    Gzip,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("compression: {0}")]
    Io(#[from] std::io::Error),
    #[error("decoded proof exceeds {0} bytes")]
    TooLarge(usize),
}

/// Encode proof bytes (bincode of a PlainProof) for the wire.
pub fn encode_proof(bytes: &[u8], enc: ProofEncoding) -> Result<String, EncodeError> {
    let raw = match enc {
        ProofEncoding::Plain => return Ok(STANDARD.encode(bytes)),
        ProofEncoding::Zstd => zstd::bulk::compress(bytes, ZSTD_LEVEL)?,
        ProofEncoding::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 2), flate2::Compression::default());
            e.write_all(bytes)?;
            e.finish()?
        }
    };
    Ok(STANDARD.encode(raw))
}

/// Decode a wire value produced by [`encode_proof`], never inflating beyond [`MAX_DECODED_PROOF`].
pub fn decode_proof(b64: &str, enc: ProofEncoding) -> Result<Vec<u8>, EncodeError> {
    if b64.len() / 4 * 3 > MAX_DECODED_PROOF {
        return Err(EncodeError::TooLarge(MAX_DECODED_PROOF));
    }
    let raw = STANDARD.decode(b64.trim())?;
    match enc {
        ProofEncoding::Plain => Ok(raw),
        ProofEncoding::Zstd => bounded_read(zstd::stream::read::Decoder::new(&raw[..])?),
        ProofEncoding::Gzip => bounded_read(flate2::read::GzDecoder::new(&raw[..])),
    }
}

/// Guess the encoding of decoded base64 bytes from its magic number.
pub fn sniff_encoding(raw: &[u8]) -> ProofEncoding {
    match raw {
        [0x28, 0xb5, 0x2f, 0xfd, ..] => ProofEncoding::Zstd,
        [0x1f, 0x8b, ..] => ProofEncoding::Gzip,
        _ => ProofEncoding::Plain,
    }
}

fn bounded_read<R: Read>(r: R) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    let mut limited = r.take(MAX_DECODED_PROOF as u64 + 1);
    limited.read_to_end(&mut out)?;
    if out.len() > MAX_DECODED_PROOF {
        return Err(EncodeError::TooLarge(MAX_DECODED_PROOF));
    }
    Ok(out)
}

/// Which submit member carries the proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofField {
    /// `plain_proof`: base64(bincode), or base64(gzip(bincode)) on Kryptex v2.
    PlainProof,
    /// `plain_proof_zst`: base64(zstd(bincode)).
    PlainProofZst,
}

impl ProofField {
    pub fn key(self) -> &'static str {
        match self {
            ProofField::PlainProof => "plain_proof",
            ProofField::PlainProofZst => "plain_proof_zst",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "plain_proof" => Some(ProofField::PlainProof),
            "plain_proof_zst" => Some(ProofField::PlainProofZst),
            _ => None,
        }
    }

    pub fn other(self) -> Self {
        match self {
            ProofField::PlainProof => ProofField::PlainProofZst,
            ProofField::PlainProofZst => ProofField::PlainProof,
        }
    }

    /// The encoding this field implies on `dialect`.
    pub fn encoding(self, dialect: Dialect) -> ProofEncoding {
        match self {
            ProofField::PlainProof => dialect.plain_field_encoding(),
            ProofField::PlainProofZst => ProofEncoding::Zstd,
        }
    }
}

/// Learns, per pool, which proof field the pool accepts. Persist it (serde) between runs.
///
/// Rule: after [`MAX_FORMAT_REJECTS`] consecutive *format* rejects on the current field, switch to
/// the other field. Any accept confirms the current field and clears the counter. Other reject
/// kinds (stale, low difficulty, ...) say nothing about the format and are ignored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofFormatLearner {
    field: ProofField,
    format_rejects: u32,
    confirmed: bool,
    switches: u32,
}

impl ProofFormatLearner {
    pub fn new(initial: ProofField) -> Self {
        ProofFormatLearner { field: initial, format_rejects: 0, confirmed: false, switches: 0 }
    }

    pub fn field(&self) -> ProofField {
        self.field
    }

    /// `true` once the pool accepted a share with the current field.
    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    pub fn switches(&self) -> u32 {
        self.switches
    }

    pub fn on_accept(&mut self) {
        self.confirmed = true;
        self.format_rejects = 0;
    }

    /// Record a format reject; returns the new field when this reject caused a switch.
    pub fn on_format_reject(&mut self) -> Option<ProofField> {
        self.format_rejects += 1;
        if self.format_rejects < MAX_FORMAT_REJECTS {
            return None;
        }
        self.field = self.field.other();
        self.format_rejects = 0;
        self.confirmed = false;
        self.switches += 1;
        Some(self.field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        // Proof-like bytes: long runs of structure plus noise, ~200 KB.
        let mut v = Vec::with_capacity(200_000);
        let mut x: u32 = 0x1234_5678;
        for i in 0..200_000u32 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push(if i % 3 == 0 { (x & 0xff) as u8 } else { (i % 7) as u8 });
        }
        v
    }

    #[test]
    fn round_trips_all_encodings() {
        for data in [vec![], vec![0u8], b"hello".to_vec(), sample()] {
            for enc in [ProofEncoding::Plain, ProofEncoding::Zstd, ProofEncoding::Gzip] {
                let s = encode_proof(&data, enc).unwrap();
                assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='));
                assert_eq!(decode_proof(&s, enc).unwrap(), data, "{enc:?}");
                if enc != ProofEncoding::Plain {
                    assert_eq!(sniff_encoding(&STANDARD.decode(&s).unwrap()), enc);
                }
            }
        }
    }

    #[test]
    fn compression_actually_compresses_structured_proofs() {
        let data = sample();
        let plain = encode_proof(&data, ProofEncoding::Plain).unwrap().len();
        let zst = encode_proof(&data, ProofEncoding::Zstd).unwrap().len();
        let gz = encode_proof(&data, ProofEncoding::Gzip).unwrap().len();
        assert!(zst < plain && gz < plain, "plain {plain} zstd {zst} gzip {gz}");
    }

    #[test]
    fn decode_refuses_bombs_and_garbage() {
        let zeros = vec![0u8; MAX_DECODED_PROOF + 10];
        let bomb = encode_proof(&zeros, ProofEncoding::Zstd).unwrap();
        assert!(matches!(decode_proof(&bomb, ProofEncoding::Zstd), Err(EncodeError::TooLarge(_))));
        let bomb = encode_proof(&zeros, ProofEncoding::Gzip).unwrap();
        assert!(matches!(decode_proof(&bomb, ProofEncoding::Gzip), Err(EncodeError::TooLarge(_))));
        assert!(decode_proof("not base64!", ProofEncoding::Plain).is_err());
        let not_zstd = encode_proof(b"plain bytes", ProofEncoding::Plain).unwrap();
        assert!(decode_proof(&not_zstd, ProofEncoding::Zstd).is_err());
    }

    #[test]
    fn fields_and_encodings() {
        assert_eq!(ProofField::PlainProof.key(), "plain_proof");
        assert_eq!(ProofField::from_key("plain_proof_zst"), Some(ProofField::PlainProofZst));
        assert_eq!(ProofField::from_key("proof"), None);
        assert_eq!(ProofField::PlainProof.encoding(Dialect::Object), ProofEncoding::Plain);
        assert_eq!(ProofField::PlainProof.encoding(Dialect::KryptexV2), ProofEncoding::Gzip);
        assert_eq!(ProofField::PlainProofZst.encoding(Dialect::Kryptex), ProofEncoding::Zstd);
    }

    #[test]
    fn learner_switches_after_three_format_rejects() {
        let mut l = ProofFormatLearner::new(ProofField::PlainProofZst);
        assert_eq!(l.on_format_reject(), None);
        assert_eq!(l.on_format_reject(), None);
        assert_eq!(l.on_format_reject(), Some(ProofField::PlainProof));
        assert_eq!(l.field(), ProofField::PlainProof);
        assert!(!l.confirmed());
        // An accept confirms and resets the counter.
        l.on_format_reject();
        l.on_format_reject();
        l.on_accept();
        assert!(l.confirmed());
        assert_eq!(l.on_format_reject(), None);
        assert_eq!(l.on_format_reject(), None);
        assert_eq!(l.field(), ProofField::PlainProof);
        assert_eq!(l.on_format_reject(), Some(ProofField::PlainProofZst));
        assert_eq!(l.switches(), 2);
        // Serde round trip (the daemon persists it).
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<ProofFormatLearner>(&s).unwrap(), l);
    }
}
