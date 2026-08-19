//! SQLite record-format encode/decode for UNION variant payloads, plus
//! turso's tag + single-column outer-record framing for UNION values.
//!
//! Turso's on-disk format for a UNION column:
//!
//! ```text
//! [tag_index: u8][outer_record: one-column SQLite record]
//! ```
//!
//! The outer record always has exactly one column. For scalar variants
//! (`UNION(i INT, …)`) that column IS the scalar. For struct variants
//! (`UNION(telegram STRUCT(…))`) it's a `Value::Blob` whose bytes are
//! themselves a SQLite record, one column per struct field.
//!
//! Serial types follow SQLite's spec: 0=NULL, 1-6=signed ints
//! (1/2/3/4/6/8 bytes), 7=f64, 8/9=literal 0/1, 12+2n=BLOB of length n,
//! 13+2n=TEXT of length n. The encoder picks the narrowest serial that
//! fits each integer (serial 1/2/3/4/5/6 by magnitude, plus the
//! literal-0/1 serials 8/9); a REAL goes out as serial 7 unless it is a
//! NaN, which Turso cannot represent and stores as NULL (serial 0), so we
//! do too — see [`encode_value`]. Both turso and ourselves accept every
//! serial on read.

use thiserror::Error;

/// Why a blob was not a well-formed SQLite record.
///
/// Every variant means the bytes are wrong, not that the layout disagrees
/// — a layout disagreement usually decodes without complaint, which is
/// what [`super`]'s module docs are about.
#[derive(Debug, Error)]
pub enum WireError {
    /// A length or payload ran off the end of the buffer.
    #[error("unexpected end of buffer while decoding")]
    UnexpectedEof,
    /// A serial type SQLite reserves and assigns no meaning to (10 or 11).
    #[error("reserved SQLite record serial type: {0}")]
    ReservedSerialType(u64),
    /// A TEXT payload's bytes were not UTF-8.
    #[error("TEXT payload was not valid UTF-8: {0}")]
    InvalidUtf8(std::string::FromUtf8Error),
    /// A UNION's outer record carried something other than the single
    /// column the framing requires.
    #[error("UNION outer record had {0} columns, expected exactly 1")]
    UnexpectedOuterColumnCount(usize),
}

// -- UNION framing (tag byte + single-column outer record) ------------------

/// Encode a UNION wire blob. `outer` is the variant's single-column value:
/// the scalar itself for scalar variants, or a `Value::Blob` containing
/// an inner struct record for struct variants.
pub fn encode_union(tag_index: u8, outer: turso::Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(tag_index);
    encode_record_into(std::slice::from_ref(&outer), &mut out);
    out
}

/// Decode a UNION wire blob to `(tag_index, outer_column_value)`.
pub fn decode_union(buf: &[u8]) -> Result<(u8, turso::Value), WireError> {
    if buf.is_empty() {
        return Err(WireError::UnexpectedEof);
    }
    let tag = buf[0];
    let mut outer = decode_record(&buf[1..])?;
    if outer.len() != 1 {
        return Err(WireError::UnexpectedOuterColumnCount(outer.len()));
    }
    Ok((tag, outer.pop().expect("len == 1 checked above")))
}

// -- SQLite record format ----------------------------------------------------

/// Encode values as one SQLite record: a header of serial types, then
/// the payloads back to back.
pub fn encode_record(values: &[turso::Value]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_record_into(values, &mut out);
    out
}

fn encode_record_into(values: &[turso::Value], out: &mut Vec<u8>) {
    let mut serial_bytes = Vec::with_capacity(values.len());
    let mut body_bytes = Vec::with_capacity(values.len() * 8);
    for v in values {
        let serial = encode_value(v, &mut body_bytes);
        write_varint(&mut serial_bytes, serial);
    }

    // The header-size varint counts itself, so its own width feeds back
    // into the value it's encoding. Walk up varint widths until we find a
    // fixed point.
    let serials_len = serial_bytes.len() as u64;
    let mut len_bytes = 1u64;
    loop {
        let header_size = serials_len + len_bytes;
        let needed = varint_len(header_size);
        if needed == len_bytes {
            write_varint(out, header_size);
            break;
        }
        len_bytes = needed;
    }
    out.extend_from_slice(&serial_bytes);
    out.extend_from_slice(&body_bytes);
}

/// Decode a SQLite record back into its column values.
///
/// The record carries storage classes and nothing else, so this cannot
/// tell a correct layout from a drifted one — it only reports bytes that
/// are malformed outright.
pub fn decode_record(buf: &[u8]) -> Result<Vec<turso::Value>, WireError> {
    let (header_size, len_consumed) = read_varint(buf)?;
    let header_end = header_size as usize;
    // The header-size varint counts *itself*, so a well-formed record always
    // has `header_size >= len_consumed`. Checking only the upper bound would
    // leave the lower one to `&buf[len_consumed..header_end]`, which panics
    // rather than erroring when the two cross — and every byte of a UNION
    // column is attacker-shaped in the sense that matters here: Turso stores
    // a blob verbatim, so `Blob([1, 0])` is a row an older binary, a hand-
    // written migration or a corrupted page can leave behind, and reading it
    // back through the derive aborted the process. That is precisely what
    // [`WireError`] exists to prevent, and what [`super::display`] promises
    // when it says a blob that does not decode comes back as `None` rather
    // than taking the caller with it.
    if header_end < len_consumed || buf.len() < header_end {
        return Err(WireError::UnexpectedEof);
    }
    let mut header_cursor = &buf[len_consumed..header_end];
    let mut body = &buf[header_end..];

    let mut values = Vec::new();
    while !header_cursor.is_empty() {
        let (serial, consumed) = read_varint(header_cursor)?;
        header_cursor = &header_cursor[consumed..];
        let (value, data_bytes) = decode_value(serial, body)?;
        body = &body[data_bytes..];
        values.push(value);
    }
    Ok(values)
}

// -- Per-value (serial type + payload) --------------------------------------

fn encode_value(v: &turso::Value, body: &mut Vec<u8>) -> u64 {
    match v {
        turso::Value::Null => 0,
        turso::Value::Integer(n) => encode_integer(*n, body),
        // Turso has no NaN REAL. Its own `Value::from_f64`
        // (`core/numeric/nonnan.rs`) folds every NaN to NULL, and it does so
        // on bind, inside `union_value`, inside `struct_pack` and again on
        // record read — so a NaN written by anything on Turso's side of the
        // wire comes back as a NULL in serial 0, with no payload.
        //
        // Writing serial 7 with the NaN bit pattern here would therefore be
        // the exact failure this module's differential test exists to catch:
        // the same logical value with two byte strings. A migration's
        // `union_value(…)` would write serial 0 and the app would bind
        // serial 7, so the app's lookup by UNION key would miss the row it
        // just migrated, its insert would succeed, and the table would grow
        // a duplicate identity — silently, because nothing compares the two
        // encodings at runtime. Reading is the mirror image: Turso's row
        // hands us a NULL where our `FromSql` expects a REAL.
        //
        // So we spell Turso's rule rather than the IEEE one: NaN is NULL.
        turso::Value::Real(f) if f.is_nan() => 0,
        turso::Value::Real(f) => {
            body.extend_from_slice(&f.to_be_bytes());
            7
        }
        turso::Value::Text(s) => {
            let bytes = s.as_bytes();
            body.extend_from_slice(bytes);
            13 + (bytes.len() as u64) * 2
        }
        turso::Value::Blob(b) => {
            body.extend_from_slice(b);
            12 + (b.len() as u64) * 2
        }
    }
}

/// Pick the narrowest SQLite serial type that fits `n`.
fn encode_integer(n: i64, body: &mut Vec<u8>) -> u64 {
    // Literal-value serials 8 / 9 carry no payload.
    if n == 0 {
        return 8;
    }
    if n == 1 {
        return 9;
    }
    // Signed-range thresholds. 24-bit: [-(1<<23), (1<<23)-1]; 48-bit:
    // [-(1<<47), (1<<47)-1]. Others use native width helpers.
    const I24_MIN: i64 = -(1 << 23);
    const I24_MAX: i64 = (1 << 23) - 1;
    const I48_MIN: i64 = -(1 << 47);
    const I48_MAX: i64 = (1 << 47) - 1;

    if (i8::MIN as i64..=i8::MAX as i64).contains(&n) {
        body.push(n as u8);
        1
    } else if (i16::MIN as i64..=i16::MAX as i64).contains(&n) {
        body.extend_from_slice(&(n as i16).to_be_bytes());
        2
    } else if (I24_MIN..=I24_MAX).contains(&n) {
        // Low 3 bytes of the big-endian i64 representation.
        body.extend_from_slice(&n.to_be_bytes()[5..]);
        3
    } else if (i32::MIN as i64..=i32::MAX as i64).contains(&n) {
        body.extend_from_slice(&(n as i32).to_be_bytes());
        4
    } else if (I48_MIN..=I48_MAX).contains(&n) {
        // Low 6 bytes of the big-endian i64 representation.
        body.extend_from_slice(&n.to_be_bytes()[2..]);
        5
    } else {
        body.extend_from_slice(&n.to_be_bytes());
        6
    }
}

fn decode_value(serial: u64, buf: &[u8]) -> Result<(turso::Value, usize), WireError> {
    match serial {
        0 => Ok((turso::Value::Null, 0)),
        1 => read_int_be(buf, 1).map(|(v, n)| (turso::Value::Integer(v), n)),
        2 => read_int_be(buf, 2).map(|(v, n)| (turso::Value::Integer(v), n)),
        3 => read_int_be(buf, 3).map(|(v, n)| (turso::Value::Integer(v), n)),
        4 => read_int_be(buf, 4).map(|(v, n)| (turso::Value::Integer(v), n)),
        5 => read_int_be(buf, 6).map(|(v, n)| (turso::Value::Integer(v), n)),
        6 => read_int_be(buf, 8).map(|(v, n)| (turso::Value::Integer(v), n)),
        7 => {
            if buf.len() < 8 {
                return Err(WireError::UnexpectedEof);
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&buf[..8]);
            Ok((turso::Value::Real(f64::from_be_bytes(arr)), 8))
        }
        8 => Ok((turso::Value::Integer(0), 0)),
        9 => Ok((turso::Value::Integer(1), 0)),
        10 | 11 => Err(WireError::ReservedSerialType(serial)),
        s if s >= 12 && s.is_multiple_of(2) => {
            let len = ((s - 12) / 2) as usize;
            if buf.len() < len {
                return Err(WireError::UnexpectedEof);
            }
            Ok((turso::Value::Blob(buf[..len].to_vec()), len))
        }
        s if s >= 13 && !s.is_multiple_of(2) => {
            let len = ((s - 13) / 2) as usize;
            if buf.len() < len {
                return Err(WireError::UnexpectedEof);
            }
            let text = String::from_utf8(buf[..len].to_vec()).map_err(WireError::InvalidUtf8)?;
            Ok((turso::Value::Text(text), len))
        }
        _ => Err(WireError::ReservedSerialType(serial)),
    }
}

/// Read an N-byte signed big-endian integer with sign-extension to i64.
fn read_int_be(buf: &[u8], n: usize) -> Result<(i64, usize), WireError> {
    if buf.len() < n {
        return Err(WireError::UnexpectedEof);
    }
    let sign_bit = buf[0] & 0x80;
    let mut acc: i64 = if sign_bit != 0 { -1 } else { 0 };
    for b in &buf[..n] {
        acc = (acc << 8) | *b as i64;
    }
    Ok((acc, n))
}

// -- SQLite varint (big-endian Huffman, 1-9 bytes) ---------------------------
//
// Bytes 1..=8 contribute their low 7 bits with the high bit as a
// continuation flag; if all eight continue, byte 9 terminates and
// contributes all 8 bits. Full u64 range fits in 9 bytes.

fn varint_len(v: u64) -> u64 {
    if v <= 0x7f {
        1
    } else if v <= 0x3fff {
        2
    } else if v <= 0x1f_ffff {
        3
    } else if v <= 0x0fff_ffff {
        4
    } else if v <= 0x07_ffff_ffff {
        5
    } else if v <= 0x03ff_ffff_ffff {
        6
    } else if v <= 0x01_ffff_ffff_ffff {
        7
    } else if v <= 0x00ff_ffff_ffff_ffff_u64 {
        8
    } else {
        9
    }
}

fn write_varint(out: &mut Vec<u8>, v: u64) {
    if v <= 0x7f {
        out.push(v as u8);
        return;
    }
    if v <= 0x00ff_ffff_ffff_ffff_u64 {
        // 2..=8 byte form: terminating byte has high bit clear (low 7 bits
        // of v), each preceding byte has high bit set (next 7 bits).
        let mut buf = [0u8; 8];
        let mut i = 7;
        buf[i] = (v & 0x7f) as u8;
        let mut rest = v >> 7;
        while rest != 0 {
            i -= 1;
            buf[i] = 0x80 | ((rest & 0x7f) as u8);
            rest >>= 7;
        }
        out.extend_from_slice(&buf[i..]);
    } else {
        // 9-byte form: first 8 bytes carry 7 bits each (all with
        // continuation bit set), 9th byte carries all 8 bits = 64 total.
        let mut buf = [0u8; 9];
        buf[8] = v as u8;
        let mut rest = v >> 8;
        for b in buf[..8].iter_mut().rev() {
            *b = 0x80 | ((rest & 0x7f) as u8);
            rest >>= 7;
        }
        out.extend_from_slice(&buf);
    }
}

fn read_varint(buf: &[u8]) -> Result<(u64, usize), WireError> {
    let mut result: u64 = 0;
    for i in 0..8 {
        let b = *buf.get(i).ok_or(WireError::UnexpectedEof)?;
        if b & 0x80 == 0 {
            return Ok(((result << 7) | b as u64, i + 1));
        }
        result = (result << 7) | (b & 0x7f) as u64;
    }
    // Reached byte 9: it contributes all 8 bits.
    let b8 = *buf.get(8).ok_or(WireError::UnexpectedEof)?;
    Ok(((result << 8) | b8 as u64, 9))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_scalar_mix() {
        let values = vec![
            turso::Value::Integer(-100),
            turso::Value::Text("hi".into()),
            turso::Value::Null,
            turso::Value::Real(std::f64::consts::PI),
            turso::Value::Blob(vec![0xde, 0xad]),
        ];
        let blob = encode_record(&values);
        let decoded = decode_record(&blob).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn union_framing_scalar() {
        let blob = encode_union(3, turso::Value::Integer(42));
        let (tag, outer) = decode_union(&blob).unwrap();
        assert_eq!(tag, 3);
        assert_eq!(outer, turso::Value::Integer(42));
    }

    #[test]
    fn union_framing_struct_wrapped_blob() {
        // Struct variants: outer column is a BLOB containing inner record.
        let inner_fields = vec![turso::Value::Integer(-100), turso::Value::Text("hi".into())];
        let inner = encode_record(&inner_fields);
        let blob = encode_union(0, turso::Value::Blob(inner));
        let (tag, outer) = decode_union(&blob).unwrap();
        assert_eq!(tag, 0);
        let inner_blob = match outer {
            turso::Value::Blob(b) => b,
            other => panic!("expected Blob, got {other:?}"),
        };
        assert_eq!(decode_record(&inner_blob).unwrap(), inner_fields);
    }

    #[test]
    fn decodes_turso_native_struct_variant() {
        // Struct-variant blob from turso for
        // `union_value('telegram', struct_pack(-100, 'hi'))`.
        let blob: &[u8] = &[0, 2, 24, 3, 1, 17, 156, 104, 105];
        let (tag, outer) = decode_union(blob).unwrap();
        assert_eq!(tag, 0);
        let inner_blob = match outer {
            turso::Value::Blob(b) => b,
            other => panic!("{other:?}"),
        };
        let fields = decode_record(&inner_blob).unwrap();
        assert_eq!(
            fields,
            vec![turso::Value::Integer(-100), turso::Value::Text("hi".into())]
        );
    }

    #[test]
    fn encoder_picks_narrowest_integer_serial() {
        // Each of these should emit a record where the one payload
        // serial-type varint matches the expected narrow width.
        let cases: &[(i64, u64)] = &[
            (0, 8),
            (1, 9),
            (-1, 1),
            (127, 1),
            (128, 2),
            (-32768, 2),
            (32768, 3),
            (-(1 << 23), 3),
            (1 << 23, 4),
            (-(1 << 31), 4),
            (1i64 << 31, 5),
            (-(1i64 << 47), 5),
            (1i64 << 47, 6),
            (i64::MAX, 6),
            (i64::MIN, 6),
        ];
        for (n, expected) in cases {
            let blob = encode_record(&[turso::Value::Integer(*n)]);
            // Header: first varint is header size, second is the
            // single column's serial type.
            let (_hdr_size, consumed) = read_varint(&blob).unwrap();
            let (serial, _) = read_varint(&blob[consumed..]).unwrap();
            assert_eq!(
                serial, *expected,
                "n={n}: expected serial {expected}, got {serial}"
            );
            // Always roundtrips back to the original value.
            assert_eq!(
                decode_record(&blob).unwrap(),
                vec![turso::Value::Integer(*n)],
                "roundtrip for n={n}"
            );
        }
    }

    #[test]
    fn decodes_turso_native_scalar_variants() {
        // Scalar-variant blobs from scalar_union_wire.rs:
        //   union_value('i', 42)    → [0, 2, 1, 42]
        //   union_value('f', 3.14)  → [1, 2, 7, <f64 big-endian>]
        //   union_value('s', 'hi')  → [2, 2, 17, 104, 105]
        let (tag, v) = decode_union(&[0, 2, 1, 42]).unwrap();
        assert_eq!((tag, v), (0, turso::Value::Integer(42)));

        let (tag, v) = decode_union(&[2, 2, 17, 104, 105]).unwrap();
        assert_eq!((tag, v), (2, turso::Value::Text("hi".into())));
    }

    #[test]
    fn varint_roundtrip_across_widths() {
        // One sentinel per varint width boundary, plus u64::MAX to
        // exercise the 9-byte form.
        let values: &[u64] = &[
            0,
            0x7f,
            0x80,
            0x3fff,
            0x4000,
            0x1f_ffff,
            0x20_0000,
            0x0fff_ffff,
            0x1000_0000,
            0x07_ffff_ffff,
            0x08_0000_0000,
            0x03ff_ffff_ffff,
            0x0400_0000_0000,
            0x01_ffff_ffff_ffff,
            0x02_0000_0000_0000,
            0x00ff_ffff_ffff_ffff,
            0x0100_0000_0000_0000,
            u64::MAX,
        ];
        for &v in values {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            assert_eq!(buf.len() as u64, varint_len(v), "len mismatch for {v:#x}");
            let (got, consumed) = read_varint(&buf).unwrap();
            assert_eq!(got, v, "roundtrip {v:#x} → {got:#x}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn record_with_large_text_payload() {
        // Pre-fix this panicked: TEXT of length N produces serial
        // type 13 + 2*N, which blew past the old 2-byte varint ceiling
        // once N exceeded ~8 KB. 20 KB comfortably clears the old limit
        // and also pushes the header-size varint past 1 byte.
        let big = "x".repeat(20_000);
        let values = vec![
            turso::Value::Integer(7),
            turso::Value::Text(big.clone()),
            turso::Value::Blob(vec![0xab; 20_000]),
        ];
        let blob = encode_record(&values);
        assert_eq!(decode_record(&blob).unwrap(), values);
    }

    #[test]
    fn union_with_large_text_variant() {
        // Struct variant whose inner record has a large TEXT field —
        // the previously-panicking real-world path.
        let big = "y".repeat(12_000);
        let inner = encode_record(&[turso::Value::Text(big.clone())]);
        let blob = encode_union(5, turso::Value::Blob(inner));
        let (tag, outer) = decode_union(&blob).unwrap();
        assert_eq!(tag, 5);
        let turso::Value::Blob(inner_bytes) = outer else {
            unreachable!("encoded as Blob");
        };
        assert_eq!(
            decode_record(&inner_bytes).unwrap(),
            vec![turso::Value::Text(big)]
        );
    }
}
