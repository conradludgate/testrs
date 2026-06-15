//! Crypto test-vector suites — the data-driven pattern testrs is built for.
//!
//! Each suite is a `cases` provider returning a `Vec` of vectors; the test runs
//! once per vector, so one failing vector is one failing case with its own name.
//! [`sha256`] uses a handful of inline known-answer vectors, while [`hmac`] parses
//! the real Project Wycheproof HMAC-SHA256 suite at collection time — exactly how
//! you'd drive a test against a vendored vector file.

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

/// HMAC-SHA256 vectors from the Project Wycheproof suite, loaded and parsed at
/// collection time. The whole file is checked: "valid" vectors must verify and
/// "invalid" ones (truncated/modified tags, wrong lengths) must not. Each case is
/// named by its Wycheproof test-case id.
///
/// Source: <https://github.com/C2SP/wycheproof> `testvectors_v1/hmac_sha256_test.json`
/// (Apache-2.0). Vendored at `vectors/hmac_sha256_test.json`.
pub mod hmac {
    use hmac::{Hmac, Mac};
    use serde::Deserialize;
    use sha2::Sha256;
    use testrs::{TestCaseName, test};

    type HmacSha256 = Hmac<Sha256>;

    /// The slice of the Wycheproof MAC schema we use; unknown fields are ignored.
    #[derive(Deserialize)]
    struct Suite {
        #[serde(rename = "testGroups")]
        test_groups: Vec<Group>,
    }

    #[derive(Deserialize)]
    struct Group {
        /// Tag length in bits; the MAC is truncated to it before comparison.
        #[serde(rename = "tagSize")]
        tag_size: usize,
        tests: Vec<RawCase>,
    }

    #[derive(Deserialize)]
    struct RawCase {
        #[serde(rename = "tcId")]
        tc_id: u32,
        comment: String,
        key: String,
        msg: String,
        tag: String,
        result: String,
    }

    /// One Wycheproof vector, flattened from its group with bytes hex-decoded.
    pub struct Vector {
        pub tc_id: u32,
        pub comment: String,
        pub key: Vec<u8>,
        pub msg: Vec<u8>,
        pub expected_tag: Vec<u8>,
        /// Expected tag length in bytes (the MAC is truncated to it).
        pub tag_len: usize,
        /// Whether the expected tag should verify (`result == "valid"`).
        pub valid: bool,
    }

    impl TestCaseName for Vector {
        fn case_name(&self) -> String {
            format!("tc{}", self.tc_id)
        }
    }

    /// Parse every vector in the vendored Wycheproof file at collection time.
    ///
    /// # Panics
    /// If the embedded JSON can't be parsed or a hex field is malformed.
    pub fn vectors() -> Vec<Vector> {
        let raw = include_str!("../vectors/hmac_sha256_test.json");
        let suite: Suite = serde_json::from_str(raw).expect("parse wycheproof HMAC-SHA256 vectors");
        let decode = |s: &str| hex::decode(s).expect("wycheproof fields are valid hex");
        suite
            .test_groups
            .into_iter()
            .flat_map(|group| {
                group.tests.into_iter().map(move |case| Vector {
                    tc_id: case.tc_id,
                    comment: case.comment,
                    key: decode(&case.key),
                    msg: decode(&case.msg),
                    expected_tag: decode(&case.tag),
                    tag_len: group.tag_size / 8,
                    valid: case.result == "valid",
                })
            })
            .collect()
    }

    #[test(cases(vector = vectors))]
    fn agrees_with_wycheproof(vector: &Vector) {
        let mut mac = HmacSha256::new_from_slice(&vector.key).expect("HMAC accepts any key length");
        mac.update(&vector.msg);
        let full = mac.finalize().into_bytes();
        let verified = full[..vector.tag_len] == vector.expected_tag[..];
        assert_eq!(
            verified,
            vector.valid,
            "tc{} ({}): expected the tag to {}",
            vector.tc_id,
            vector.comment,
            if vector.valid {
                "verify"
            } else {
                "be rejected"
            },
        );
    }
}
