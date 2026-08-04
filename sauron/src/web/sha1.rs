//! SHA-1, for one purpose: the WebSocket opening handshake.
//!
//! RFC 6455 requires the server to answer `Sec-WebSocket-Key` with the base64
//! of `SHA1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")`. There is no
//! negotiating this away and no other digest is accepted, so a server that
//! speaks WebSocket needs SHA-1 whether or not anyone wanted it in the build.
//!
//! `sha2` is already a dependency and is the wrong algorithm -- SHA-2 is a
//! different function, not a newer spelling of this one. The alternatives were a
//! second digest crate or a websocket crate that brings its own; ninety lines
//! with published test vectors is cheaper than either, and `Cargo.toml`'s
//! standalone-sidecar rule is worth ninety lines.
//!
//! NOT FOR ANYTHING ELSE
//! ---------------------
//! SHA-1 is broken for collision resistance and has been since 2017. It is safe
//! here because the handshake uses it as a *fixed transformation of a public
//! value* -- proof that the peer parsed the header, not a security claim about
//! anything. Do not reach for this to hash a password, sign a payload, or
//! deduplicate content; `sha2` is right there and `store.rs` already uses it.
//!
//! grep targets:
//!   fn digest       -- bytes in, twenty bytes out
//!   fn accept_key   -- the handshake answer, base64 and all

/// SHA-1 of `data`, as the twenty raw bytes.
pub fn digest(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    // Message + 0x80 + zero padding to 56 mod 64 + the length in bits, big-endian.
    let mut msg = data.to_vec();
    let bits = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`.
///
/// The GUID is from RFC 6455 and is not a secret, a salt, or configurable; it
/// exists so that a cache or a proxy replaying an unrelated base64 string cannot
/// accidentally complete a handshake.
pub fn accept_key(client_key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    crate::plat::base64(&digest(format!("{client_key}{GUID}").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: [u8; 20]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(hex(digest(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(digest(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            "the 56-byte case: one byte of padding short of a second block"
        );
        // A million 'a'. The only vector that exercises the multi-block path
        // hard enough to catch a broken length or a mis-chunked buffer.
        assert_eq!(
            hex(digest(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn the_handshake_answer_matches_rfc_6455() {
        // The example key and accept value from RFC 6455 section 1.3. If this
        // passes, every browser's handshake passes.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
