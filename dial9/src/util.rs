//! Small helpers shared by the tunnel, endpoint, and controller modes.

use eyre::{Result, bail};

/// Decodes a 32-byte value passed as hexadecimal on the command line.
pub fn decode_key(value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value).or_else(|e| bail!("invalid hexadecimal key: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| eyre::eyre!("key must be 32 bytes"))
}

/// Linux interface names: 1 through 15 bytes, ASCII letters, digits, `_`, `-`, `.`.
pub fn validate_interface_name(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 15 {
        return Err("interface name must contain 1 through 15 bytes".to_owned());
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(
            "interface name may contain only ASCII letters, digits, '_', '-', and '.'".to_owned(),
        );
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_must_be_exactly_32_bytes() {
        assert!(decode_key(&"01".repeat(32)).is_ok());
        assert!(decode_key(&"01".repeat(31)).is_err());
        assert!(decode_key("not hex").is_err());
    }

    #[test]
    fn interface_name_validation_accepts_linux_safe_names() {
        assert_eq!(validate_interface_name("d9gotatun0").unwrap(), "d9gotatun0");
        assert!(validate_interface_name("").is_err());
        assert!(validate_interface_name("sixteen-byte-name").is_err());
        assert!(validate_interface_name("bad/name").is_err());
    }
}
