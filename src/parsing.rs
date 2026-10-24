//! Dependency-free parsing helpers shared by `config` and `security::wallet`.
//!
//! Everything here is pure `std`: no allocation of secret-bearing intermediates,
//! no indexing, no panics.

/// Lamports in one SOL.
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Number of decimal places in a SOL amount (1 lamport = 1e-9 SOL).
const SOL_DECIMALS: usize = 9;

/// Whether `host` (as `Url::host_str` renders it, so IPv6 keeps its brackets)
/// is this machine: `localhost`, or any loopback IP address.
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// The endpoint transport policy (audit finding #47): `https` anywhere,
/// plain `http` only towards a loopback host (local mocks and tunnels). Any
/// other scheme is refused.
#[must_use]
pub fn endpoint_scheme_allowed(scheme: &str, host: &str) -> bool {
    match scheme {
        "https" => true,
        "http" => is_loopback_host(host),
        _ => false,
    }
}

fn is_ascii_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Converts a plain decimal SOL amount (e.g. `"0.5"`) into lamports using
/// exact integer arithmetic (no floating point, so no rounding surprises).
///
/// Accepted: digits with an optional fractional part of at most 9 digits.
/// Rejected: signs, exponents, empty parts (`".5"`, `"1."`), more than 9
/// decimals, and values that overflow `u64` lamports.
///
/// Zero is accepted here; range policy belongs to the caller.
///
/// # Errors
/// Returns a static, value-free description of what is wrong.
pub fn parse_sol_to_lamports(raw: &str) -> Result<u64, &'static str> {
    const FORMAT: &str = "expected a plain decimal number such as `0.5`";
    const TOO_LARGE: &str = "value is too large";

    let (whole, fraction, has_point) = match raw.split_once('.') {
        Some((whole, fraction)) => (whole, fraction, true),
        None => (raw, "", false),
    };

    if !is_ascii_digits(whole) || (has_point && !is_ascii_digits(fraction)) {
        return Err(FORMAT);
    }
    if fraction.len() > SOL_DECIMALS {
        return Err("at most 9 decimal places are allowed (1 lamport precision)");
    }

    let whole_lamports = whole
        .parse::<u64>()
        .map_err(|_| TOO_LARGE)?
        .checked_mul(LAMPORTS_PER_SOL)
        .ok_or(TOO_LARGE)?;

    // Right-pad the fraction to exactly 9 digits: "5" -> "500000000" lamports.
    let padded = format!("{fraction:0<width$}", width = SOL_DECIMALS);
    let fraction_lamports = padded.parse::<u64>().map_err(|_| FORMAT)?;

    whole_lamports
        .checked_add(fraction_lamports)
        .ok_or(TOO_LARGE)
}

/// Parses a human friendly boolean (`true/false`, `1/0`, `yes/no`, `on/off`),
/// case-insensitively. Returns `None` for anything else.
#[must_use]
pub fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Why a textual byte array could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteArrayError {
    /// Not a well-formed `[0-255, ...]` list.
    Malformed,
    /// Well-formed, but the element count differs from the target size.
    WrongLength {
        /// Number of elements found in the input.
        actual: usize,
    },
}

/// Decodes a JSON-style byte array such as `[1, 2, 3]` directly into `out`.
///
/// This deliberately avoids `serde_json::from_str::<Vec<u8>>`: a growing `Vec`
/// reallocates, leaving un-wiped copies of key material in freed heap memory.
/// Here the only destination is the caller-provided fixed buffer (which the
/// caller is expected to wrap in `Zeroizing`).
///
/// On error `out` may be partially written; the caller must discard it.
///
/// # Errors
/// - [`ByteArrayError::Malformed`] for missing brackets, empty/non-numeric
///   tokens, signs, trailing commas, or values above 255.
/// - [`ByteArrayError::WrongLength`] when the count differs from `N`.
pub fn decode_byte_array<const N: usize>(
    text: &str,
    out: &mut [u8; N],
) -> Result<(), ByteArrayError> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or(ByteArrayError::Malformed)?
        .trim();

    if inner.is_empty() {
        return Err(ByteArrayError::WrongLength { actual: 0 });
    }

    let mut count: usize = 0;
    for token in inner.split(',') {
        let token = token.trim();
        if !is_ascii_digits(token) {
            return Err(ByteArrayError::Malformed);
        }
        let value = token.parse::<u8>().map_err(|_| ByteArrayError::Malformed)?;
        if let Some(slot) = out.get_mut(count) {
            *slot = value;
        }
        count = count.saturating_add(1);
    }

    if count == N {
        Ok(())
    } else {
        Err(ByteArrayError::WrongLength { actual: count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sol_amounts_convert_exactly() {
        assert_eq!(parse_sol_to_lamports("1"), Ok(1_000_000_000));
        assert_eq!(parse_sol_to_lamports("0.5"), Ok(500_000_000));
        assert_eq!(parse_sol_to_lamports("0.000000001"), Ok(1));
        assert_eq!(parse_sol_to_lamports("0"), Ok(0));
        assert_eq!(parse_sol_to_lamports("0.0"), Ok(0));
        assert_eq!(parse_sol_to_lamports("12.345678901"), Ok(12_345_678_901));
        assert_eq!(parse_sol_to_lamports("007.50"), Ok(7_500_000_000));
        // Classic f64 trap: 0.1 + 0.2 style values must stay exact.
        assert_eq!(parse_sol_to_lamports("0.3"), Ok(300_000_000));
        assert_eq!(parse_sol_to_lamports("1.999999999"), Ok(1_999_999_999));
    }

    #[test]
    fn sol_amounts_reject_bad_input() {
        for bad in [
            "",
            ".",
            ".5",
            "1.",
            "-1",
            "+1",
            "1e3",
            "abc",
            "1,5",
            "1.2.3",
            " 1",
            "1 ",
            "0x10",
            "1.0000000001",
        ] {
            assert!(parse_sol_to_lamports(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn sol_amounts_reject_overflow() {
        // u64::MAX lamports is ~18_446_744_073.709551615 SOL.
        assert!(parse_sol_to_lamports("18446744073").is_ok());
        assert!(parse_sol_to_lamports("18446744074").is_err());
        assert!(parse_sol_to_lamports("99999999999999999999999").is_err());
    }

    #[test]
    fn booleans_parse_case_insensitively() {
        for yes in ["true", "TRUE", "1", "Yes", "on", " ON "] {
            assert_eq!(parse_bool(yes), Some(true), "{yes:?}");
        }
        for no in ["false", "False", "0", "NO", "off"] {
            assert_eq!(parse_bool(no), Some(false), "{no:?}");
        }
        for bad in ["", "2", "maybe", "truee"] {
            assert_eq!(parse_bool(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn byte_arrays_decode_into_fixed_buffers() {
        let mut out = [0u8; 4];
        assert_eq!(decode_byte_array("[1,2,3,255]", &mut out), Ok(()));
        assert_eq!(out, [1, 2, 3, 255]);

        let mut out = [0u8; 3];
        assert_eq!(decode_byte_array("  [ 0 , 10,\n 200 ]  ", &mut out), Ok(()));
        assert_eq!(out, [0, 10, 200]);
    }

    #[test]
    fn byte_arrays_report_length_and_format_errors() {
        let mut out = [0u8; 3];
        assert_eq!(
            decode_byte_array("[]", &mut out),
            Err(ByteArrayError::WrongLength { actual: 0 })
        );
        assert_eq!(
            decode_byte_array("[1,2]", &mut out),
            Err(ByteArrayError::WrongLength { actual: 2 })
        );
        assert_eq!(
            decode_byte_array("[1,2,3,4,5]", &mut out),
            Err(ByteArrayError::WrongLength { actual: 5 })
        );
        for bad in [
            "1,2,3",
            "[1,2,3",
            "1,2,3]",
            "[1,2,]",
            "[,1,2]",
            "[1,,2]",
            "[1,2,256]",
            "[1,2,-3]",
            "[1,2,+3]",
            "[1,2,x]",
            "[[1,2,3]]",
            "[1.5,2,3]",
            "[1,2,3] x",
        ] {
            assert_eq!(
                decode_byte_array(bad, &mut out),
                Err(ByteArrayError::Malformed),
                "should reject {bad:?}"
            );
        }
    }
}
