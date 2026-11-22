use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

pub type ProtoMap<'a> = BTreeMap<u64, Vec<ProtoValue<'a>>>;

/// Generic protobuf wire-format decoder.
/// Never panics on truncated input, oversized varints/lengths, or invalid wire tags;
/// stops decoding gracefully when malformed bytes are encountered.
pub fn decode_fields(bytes: &[u8]) -> ProtoMap<'_> {
    let mut result: ProtoMap<'_> = BTreeMap::new();
    if bytes.is_empty() {
        return result;
    }

    let mut offset: usize = 0;

    while offset < bytes.length_or_len() {
        let Some(tag) = read_varint(bytes, &mut offset) else {
            break;
        };
        let field_number = tag >> 3;
        let wire = tag & 0x07;
        if field_number == 0 {
            break;
        }

        match wire {
            0 => {
                let Some(v) = read_varint(bytes, &mut offset) else {
                    break;
                };
                result
                    .entry(field_number)
                    .or_default()
                    .push(ProtoValue::Varint(v));
            }
            1 => {
                if bytes.len().saturating_sub(offset) < 8 {
                    break;
                }
                offset += 8;
            }
            5 => {
                if bytes.len().saturating_sub(offset) < 4 {
                    break;
                }
                offset += 4;
            }
            2 => {
                let Some(len_u64) = read_varint(bytes, &mut offset) else {
                    break;
                };
                let Ok(len) = usize::try_from(len_u64) else {
                    break;
                };
                if len > bytes.len().saturating_sub(offset) {
                    break;
                }
                let slice = &bytes[offset..offset + len];
                offset += len;
                result
                    .entry(field_number)
                    .or_default()
                    .push(ProtoValue::Bytes(slice));
            }
            _ => {
                // Unsupported/deprecated wire types (3, 4, 6, 7)
                break;
            }
        }
    }

    result
}

trait SliceLen {
    fn length_or_len(&self) -> usize;
}

impl SliceLen for [u8] {
    fn length_or_len(&self) -> usize {
        self.len()
    }
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for shift in (0..70).step_by(7) {
        if *offset >= bytes.len() {
            return None;
        }
        let byte = bytes[*offset];
        *offset += 1;
        if shift == 63 && byte > 1 {
            // 10th byte cannot overflow 64-bit unsigned integer or have continuation bit
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

pub fn proto_num(map: &ProtoMap<'_>, key: u64) -> u64 {
    match map.get(&key).and_then(|v| v.first()) {
        Some(ProtoValue::Varint(n)) => *n,
        _ => 0,
    }
}

pub fn proto_bytes<'a>(map: &ProtoMap<'a>, key: u64) -> Option<&'a [u8]> {
    match map.get(&key).and_then(|v| v.first()) {
        Some(ProtoValue::Bytes(b)) => Some(*b),
        _ => None,
    }
}

pub fn proto_nested<'a>(map: &ProtoMap<'a>, key: u64) -> ProtoMap<'a> {
    match proto_bytes(map, key) {
        Some(b) => decode_fields(b),
        None => BTreeMap::new(),
    }
}

pub fn proto_text(map: &ProtoMap<'_>, key: u64) -> String {
    let Some(b) = proto_bytes(map, key) else {
        return String::new();
    };
    match std::str::from_utf8(b) {
        Ok(s) => s.trim().to_string(),
        Err(_) => String::new(),
    }
}
