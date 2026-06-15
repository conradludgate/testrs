//! Crypto test-vector suites — the data-driven pattern testrs is built for.
//!
//! Each suite is a `cases` provider returning a `Vec` of vectors (parsed at
//! collection time; here inline, in practice from a `.rsp`/JSON file via
//! `include_str!`). The test runs once per vector, so one failing vector is one
//! failing case with its own name. The vectors are real published values: the
//! SHA-256 examples and the HMAC-SHA256 cases from RFC 4231.

/// SHA-256 known-answer vectors, each named by its description via `TestCaseName`.
pub mod sha256 {
    use sha2::{Digest, Sha256};
    use testrs::{TestCaseName, test};

    pub struct Vector {
        pub name: &'static str,
        pub input: &'static [u8],
        pub digest: &'static str,
    }

    impl TestCaseName for Vector {
        fn case_name(&self) -> String {
            self.name.to_owned()
        }
    }

    pub fn vectors() -> Vec<Vector> {
        vec![
            Vector {
                name: "empty",
                input: b"",
                digest: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            },
            Vector {
                name: "abc",
                input: b"abc",
                digest: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            },
            Vector {
                name: "two_block",
                input: b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                digest: "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            },
            Vector {
                name: "quick_brown_fox",
                input: b"The quick brown fox jumps over the lazy dog",
                digest: "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
            },
        ]
    }

    #[test(cases(vector = vectors))]
    fn digest_matches(vector: &Vector) {
        let digest = Sha256::digest(vector.input);
        assert_eq!(hex::encode(digest), vector.digest);
    }
}

/// HMAC-SHA256 vectors from RFC 4231, each named by its RFC case number.
pub mod hmac {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use testrs::{TestCaseName, test};

    type HmacSha256 = Hmac<Sha256>;

    pub struct Vector {
        pub rfc_case: u8,
        pub key: &'static [u8],
        pub data: &'static [u8],
        pub mac: &'static str,
    }

    impl TestCaseName for Vector {
        fn case_name(&self) -> String {
            format!("rfc4231_{}", self.rfc_case)
        }
    }

    pub fn vectors() -> Vec<Vector> {
        vec![
            Vector {
                rfc_case: 1,
                key: &[0x0b; 20],
                data: b"Hi There",
                mac: "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            },
            Vector {
                rfc_case: 2,
                key: b"Jefe",
                data: b"what do ya want for nothing?",
                mac: "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            },
            Vector {
                rfc_case: 3,
                key: &[0xaa; 20],
                data: &[0xdd; 50],
                mac: "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            },
        ]
    }

    #[test(cases(vector = vectors))]
    fn mac_matches(vector: &Vector) {
        let mut mac = HmacSha256::new_from_slice(vector.key).expect("any key length is valid");
        mac.update(vector.data);
        let tag = mac.finalize().into_bytes();
        assert_eq!(hex::encode(tag), vector.mac);
    }
}
