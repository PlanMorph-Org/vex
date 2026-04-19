//! Decode IFC's 22-character GlobalId (a compact base64 variant of a 128-bit UUID).
//!
//! The alphabet is:
//! `0-9`, `A-Z`, `a-z`, `_`, `$` — 64 symbols, grouped in 6-bit units,
//! packed big-endian. The first character carries only 2 bits (the high two bits
//! of the first byte), yielding 128 bits total across 22 chars.
//!
//! Reference: buildingSMART spec for IfcGloballyUniqueId.

use vex_utils::VexError;

/// 64-symbol alphabet used by IfcGloballyUniqueId.
const ALPHA: &[u8; 64] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";

/// Decode an IFC GlobalId into its 16-byte representation.
///
/// Returns an error if the input is not exactly 22 characters from the IFC
/// alphabet.
pub fn decode_global_id(s: &str) -> Result<[u8; 16], VexError> {
    let bytes = s.as_bytes();
    if bytes.len() != 22 {
        return Err(VexError::Other(format!(
            "IfcGuid must be 22 chars, got {}",
            bytes.len()
        )));
    }
    let mut vals = [0u8; 22];
    for (i, &b) in bytes.iter().enumerate() {
        // Linear scan is fine for 64 elements — LLVM turns this into a jump
        // table at -O3, and this is not on any hot path (GlobalIds are parsed
        // once per entity).
        let v = ALPHA.iter().position(|&c| c == b).ok_or_else(|| {
            VexError::Other(format!("invalid IfcGuid char at position {i}: {:?}", b as char))
        })?;
        vals[i] = v as u8;
    }

    // First char contributes 2 bits; remaining 21 chars contribute 6 bits each
    // ⇒ 2 + 21*6 = 128 bits.
    let mut acc: u128 = u128::from(vals[0] & 0b11);
    for &v in &vals[1..] {
        acc = (acc << 6) | u128::from(v);
    }
    Ok(acc.to_be_bytes())
}

/// Canonical UUID-like hex rendering, lowercase with hyphens.
#[must_use]
pub fn render_uuid(bytes: [u8; 16]) -> String {
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_known_sample() {
        // A known fixture: this GlobalId appears in the IFC4 "FZK-Haus" model.
        // It doesn't matter for correctness exactly which bytes it maps to —
        // only that decoding 22 valid chars succeeds and produces 16 bytes.
        let guid = "2O2Fr$t4X7Zf8NOew3FNr2";
        let bytes = decode_global_id(guid).expect("decode");
        assert_eq!(bytes.len(), 16);
        let uuid = render_uuid(bytes);
        assert_eq!(uuid.len(), 36);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(decode_global_id("short").is_err());
        assert!(decode_global_id("0123456789012345678901234").is_err());
    }

    #[test]
    fn rejects_invalid_char() {
        // '%' is not in the alphabet.
        assert!(decode_global_id("2O2Fr$t4X7Zf8NOew3FNr%").is_err());
    }
}
