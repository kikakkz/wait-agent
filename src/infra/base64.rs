use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64Error {
    message: String,
}

impl Base64Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Base64Error {}

pub fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut encoded = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);

        encoded.push(TABLE[(a >> 2) as usize] as char);
        encoded.push(TABLE[(((a & 0b0000_0011) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((b & 0b0000_1111) << 2) | (c >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(c & 0b0011_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

pub fn decode_base64(value: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = value.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(Base64Error::new(
            "base64 input length must be divisible by 4",
        ));
    }

    let mut decoded = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks(4) {
        let a = decode_base64_value(chunk[0])?;
        let b = decode_base64_value(chunk[1])?;
        let c = decode_base64_padding(chunk[2])?;
        let d = decode_base64_padding(chunk[3])?;

        decoded.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            decoded.push(((b & 0b0000_1111) << 4) | (c.unwrap_or(0) >> 2));
        }
        if chunk[3] != b'=' {
            decoded.push(((c.unwrap_or(0) & 0b0000_0011) << 6) | d.unwrap_or(0));
        }

        if chunk[2] == b'=' && chunk[3] != b'=' {
            return Err(Base64Error::new(
                "base64 input cannot use padding in the third position only",
            ));
        }
    }

    Ok(decoded)
}

fn decode_base64_padding(byte: u8) -> Result<Option<u8>, Base64Error> {
    if byte == b'=' {
        Ok(None)
    } else {
        decode_base64_value(byte).map(Some)
    }
}

fn decode_base64_value(byte: u8) -> Result<u8, Base64Error> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        other => Err(Base64Error::new(format!(
            "invalid base64 character `{}`",
            other as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_base64, encode_base64};

    #[test]
    fn base64_round_trips_ascii_and_utf8_bytes() {
        for bytes in [b"a".as_slice(), b"abc", "你好".as_bytes()] {
            let encoded = encode_base64(bytes);
            let decoded = decode_base64(&encoded).expect("base64 should decode");
            assert_eq!(decoded, bytes);
        }
    }

    #[test]
    fn base64_rejects_invalid_padding_shape() {
        let decoded = decode_base64("AAA=").expect("base64 with valid padding should decode");
        assert_eq!(decoded, vec![0, 0]);

        let error = decode_base64("AA=A").expect_err("invalid padding should fail");
        assert_eq!(
            error.to_string(),
            "base64 input cannot use padding in the third position only"
        );
    }
}
