//! Test vectors and cross-checks for QuickXorHash.
//!
//! Vectors are taken from rclone's QuickXorHash tests (MIT licensed,
//! <https://github.com/rclone/rclone/blob/master/backend/onedrive/quickxorhash/quickxorhash_test.go>),
//! which were generated with Microsoft's reference implementation.

use base64::Engine as _;
use oneup_hash::{QuickXorHash, hash_reader, quick_xor_hash_base64};
use proptest::prelude::*;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// (input length, base64 input, base64 expected hash)
const VECTORS: &[(usize, &str, &str)] = &[
    (0, "", "AAAAAAAAAAAAAAAAAAAAAAAAAAA="),
    (1, "Sg==", "SgAAAAAAAAAAAAAAAQAAAAAAAAA="),
    (2, "tbQ=", "taAFAAAAAAAAAAAAAgAAAAAAAAA="),
    (3, "0pZP", "0rDEEwAAAAAAAAAAAwAAAAAAAAA="),
    (4, "jRRDVA==", "jaDAEKgAAAAAAAAABAAAAAAAAAA="),
    (5, "eAV52qE=", "eChAHrQRCgAAAAAABQAAAAAAAAA="),
    (6, "luBZlaT6", "lgBHFipBCn0AAAAABgAAAAAAAAA="),
    (7, "qaApEj66lw==", "qQBFCiTgA11cAgAABwAAAAAAAAA="),
    (8, "/aNzzCFPS/A=", "/RjFHJgRgicsAR4ACAAAAAAAAAA="),
    (9, "n6Neh7p6fFgm", "nxiFFw6hCz3wAQsmCQAAAAAAAAA="),
    (10, "J9iPGCbfZSTNyw==", "J8DGIzBggm+UgQTNUgYAAAAAAAA="),
    (11, "i+UZyUGJKh+ISbk=", "iyhHBpIRhESo4AOIQ0IuAAAAAAA="),
    (12, "h490d57Pqz5q2rtT", "h3gEHe7giWeswgdq3MYupgAAAAA="),
    (13, "vPgoDjOfO6fm71RxLw==", "vMAHChwwg0/s4BTmdQcV4vACAAA="),
    (14, "XoJ1AsoR4fDYJrDqYs4=", "XhBEHQSgjAiEAx7YPgEs1CEGZwA="),
    (15, "gQaybEqS/4UlDc8e4IJm", "gDCALNigBEn8oxAlZ8AzPAAOQZg="),
    (16, "2fuxhBJXtpWFe8dOfdGeHw==", "O9tHLAghgSvYohKFyMMxnNCHaHg="),
    (17, "XBV6YKU9V7yMakZnFIxIkuU=", "HbplHsBQih5cgReMQYMRzkABRiA="),
    (18, "XJZSOiNO2bmfKnTKD7fztcQX", "/6ZArHQwAidkIxefQgEdlPGAW8w="),
    (19, "g8VtAh+2Kf4k0kY5tzji2i2zmA==", "wDNrgwHWAVukwB8kg4YRcnALHIg="),
    (20, "T6LYJIfDh81JrAK309H2JMJTXis=", "zBTHrspn3mEcohlJdIUAbjGNaNg="),
    (21, "DWAAX5/CIfrmErgZa8ot6ZraeSbu", "LR2Z0PjuRYGKQB/mhQAuMrAGZbQ="),
    (22, "N9abi3qy/mC1THZuVLHPpx7SgwtLOA==", "1KTYttCBEen8Hwy1doId3ECFWDw="),
    (23, "LlUe7wHerLqEtbSZLZgZa9u0m7hbiFs=", "TqVZpxs3cN61BnuFvwUtMtECTGQ="),
    (24, "bU2j/0XYdgfPFD4691jV0AOUEUPR4Z5E", "bnLBiLpVgnxVkXhNsIAPdHAPLFQ="),
    (25, "lScPwPsyUsH2T1Qsr31wXtP55Wqbe47Uyg==", "VDMSy8eI26nBHCB0e8gVWPCKPsA="),
    (26, "rJaKh1dLR1k+4hynliTZMGf8Nd4qKKoZiAM=", "r7bjwkl8OYQeNaMcCY8fTmEJEmQ="),
    (27, "pPsT0CPmHrd3Frsnva1pB/z1ytARLeHEYRCo", "Rdg7rCcDomL59pL0s6GuTvqLVqQ="),
    (28, "wSRChaqmrsnMrfB2yqI43eRWbro+f9kBvh+01w==", "YTtloIi6frI7HX3vdLvE7I2iUOA="),
    (29, "apL67KMIRxQeE9k1/RuW09ppPjbF1WeQpTjSWtI=", "CIpedls+ZlSQ654fl+X26+Q7LVU="),
    (30, "53yx0/QgMTVb7OOzHRHbkS7ghyRc+sIXxi7XHKgT", "zfJtLGFgR9DB3Q64fAFIp+S5iOY="),
    (31, "PwXNnutoLLmxD8TTog52k8cQkukmT87TTnDipKLHQw==", "PTaGs7yV3FUyBy/SfU6xJRlCJlI="),
    (32, "NbYXsp5/K6mR+NmHwExjvWeWDJFnXTKWVlzYHoesp2E=", "wjuAuWDiq04qDt1R8hHWDDcwVoQ="),
    (33, "qQ70RB++JAR5ljNv3lJt1PpqETPsckopfonItu18Cr3E", "FkJaeg/0Z5+euShYlLpE2tJh+Lo="),
    (34, "RhzSatQTQ9/RFvpHyQa1WLdkr3nIk6MjJUma998YRtp44A==", "SPN2D29reImAqJezlqV2DLbi8tk="),
    (35, "DND1u1uZ5SqZVpRUk6NxSUdVo7IjjL9zs4A1evDNCDLcXWc=", "S6lBk2hxI2SWBfn7nbEl7D19UUs="),
    (36, "jEi62utFz69JMYHjg1iXy7oO6ZpZSLcVd2B+pjm6BGsv/CWi", "s0lYU9tr/bp9xsnrrjYgRS5EvV8="),
    (37, "hfS3DZZnhy0hv7nJdXLv/oJOtIgAuP9SInt/v8KeuO4/IvVh4A==", "CV+HQCdd2A/e/vdi12f2UU55GLA="),
    (38, "EkPQAC6ymuRrYjIXD/LT/4Vb+7aTjYVZOHzC8GPCEtYDP0+T3Nc=", "kE9H9sEmr3vHBYUiPbvsrcDgSEo="),
    (39, "vtBOGIENG7yQ/N7xNWPNIgy66Gk/I2Ur/ZhdFNUK9/1FCZuu/KeS", "+Fgp3HBimtCzUAyiinj3pkarYTk="),
    (40, "YnF4smoy9hox2jBlJ3VUa4qyCRhOZbWcmFGIiszTT4zAdYHsqJazyg==", "arkIn+ELddmE8N34J9ydyFKW+9w="),
    (41, "0n7nl3YJtipy6yeUbVPWtc2h45WbF9u8hTz5tNwj3dZZwfXWkk+GN3g=", "YJLNK7JR64j9aODWfqDvEe/u6NU="),
    (42, "FnIIPHayc1pHkY4Lh8+zhWwG8xk6Knk/D3cZU1/fOUmRAoJ6CeztvMOL", "22RPOylMtdk7xO/QEQiMli4ql0k="),
    (43, "J82VT7ND0Eg1MorSfJMUhn+qocF7PsUpdQAMrDiHJ2JcPZAHZ2nyuwjoKg==", "pOR5eYfwCLRJbJsidpc1rIJYwtM="),
    (44, "Zbu+78+e35ZIymV5KTDdub5McyI3FEO8fDxs62uWHQ9U3Oh3ZqgaZ30SnmQ=", "DbvbTkgNTgWRqRidA9r1jhtUjro="),
    (45, "lgybK3Da7LEeY5aeeNrqcdHvv6mD1W4cuQ3/rUj2C/CNcSI0cAMw6vtpVY3y", "700RQByn1lRQSSme9npQB/Ye+bY="),
    (46, "jStZgKHv4QyJLvF2bYbIUZi/FscHALfKHAssTXkrV1byVR9eACwW9DNZQRHQwg==", "uwN55He8xgE4g93dH9163xPew4U="),
    (47, "V1PSud3giF5WW72JB/bgtltsWtEB5V+a+wUALOJOGuqztzVXUZYrvoP3XV++gM0=", "U+3ZfUF/6mwOoHJcSHkQkckfTDA="),
    (48, "VXs4t4tfXGiWAL6dlhEMm0YQF0f2w9rzX0CvIVeuW56o6/ec2auMpKeU2VeteEK5", "sq24lSf7wXLH8eigHl07X+qPTps="),
    (49, "bLUn3jLH+HFUsG3ptWTHgNvtr3eEv9lfKBf0jm6uhpqhRwtbEQ7Ovj/hYQf42zfdtQ==", "uC8xrnopGiHebGuwgq607WRQyxQ="),
    (50, "4SVmjtXIL8BB8SfkbR5Cpaljm2jpyUfAhIBf65XmKxHlz9dy5XixgiE/q1lv+esZW/E=", "wxZ0rxkMQEnRNAp8ZgEZLT4RdLM="),
    (51, "pMljctlXeFUqbG3BppyiNbojQO3ygg6nZPeUZaQcVyJ+Clgiw3Q8ntLe8+02ZSfyCc39", "aZEPmNvOXnTt7z7wt+ewV7QGMlg="),
    (52, "C16uQlxsHxMWnV2gJhFPuJ2/guZ4N1YgmNvAwL1yrouGQtwieGx8WvZsmYRnX72JnbVtTw==", "QtlSNqXhVij64MMhKJ3EsDFB/z8="),
    (53, "7ZVDOywvrl3L0GyKjjcNg2CcTI81n2CeUbzdYWcZOSCEnA/xrNHpiK01HOcGh3BbxuS4S6g=", "4NznNJc4nmXeApfiCFTq/H5LbHw="),
    (54, "JXm2tTVqpYuuz2Cc+ZnPusUb8vccPGrzWK2oVwLLl/FjpFoxO9FxGlhnB08iu8Q/XQSdzHn+", "IwE5+2pKNcK366I2k2BzZYPibSI="),
    (
        55,
        "TiiU1mxzYBSGZuE+TX0l9USWBilQ7dEml5lLrzNPh75xmhjIK8SGqVAkvIMgAmcMB+raXdMPZg==",
        "yECGHtgR128ScP4XlvF96eLbIBE=",
    ),
    (
        56,
        "zz+Q4zi6wh0fCJUFU9yUOqEVxlIA93gybXHOtXIPwQQ44pW4fyh6BRgc1bOneRuSWp85hwlTJl8=",
        "+3Ef4D6yuoC8J+rbFqU1cegverE=",
    ),
    (
        57,
        "sa6SHK9z/G505bysK5KgRO2z2cTksDkLoFc7sv0tWBmf2G2mCiozf2Ce6EIO+W1fRsrrtn/eeOAV",
        "xZg1CwMNAjN0AIXw2yh4+1N3oos=",
    ),
];

/// Deliberately naive, bit-at-a-time reimplementation from the algorithm
/// description, used as an independent oracle for long inputs.
fn reference(data: &[u8]) -> [u8; 20] {
    let mut bits = [false; 160];
    for (n, &byte) in data.iter().enumerate() {
        let start = (n * 11) % 160;
        for j in 0..8 {
            if byte >> j & 1 == 1 {
                let p = (start + j) % 160;
                bits[p] = !bits[p];
            }
        }
    }
    let mut out = [0u8; 20];
    for (p, &set) in bits.iter().enumerate() {
        if set {
            out[p / 8] |= 1 << (p % 8);
        }
    }
    for (i, l) in (data.len() as u64).to_le_bytes().iter().enumerate() {
        out[12 + i] ^= l;
    }
    out
}

#[test]
fn rclone_vectors() {
    assert!(VECTORS.len() >= 50);
    for &(len, input, expected) in VECTORS {
        let data = B64.decode(input).expect("vector input is valid base64");
        assert_eq!(data.len(), len);
        assert_eq!(quick_xor_hash_base64(&data), expected, "vector of length {len}");
        assert_eq!(B64.encode(reference(&data)), expected, "reference, length {len}");
    }
}

#[test]
fn empty_input() {
    let h = QuickXorHash::new();
    assert!(h.is_empty());
    assert_eq!(h.finalize_base64(), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
}

#[test]
fn reader_matches_update() {
    let data: Vec<u8> = (0..100_003u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let mut buf = [0u8; 4096];
    let h = hash_reader(&data[..], &mut buf).expect("in-memory read cannot fail");
    assert_eq!(h.len(), data.len() as u64);
    assert_eq!(h.finalize(), reference(&data));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Long inputs (crossing many 160-byte wraps) agree with the oracle.
    #[test]
    fn matches_reference(data in proptest::collection::vec(any::<u8>(), 0..2000)) {
        let mut h = QuickXorHash::new();
        h.update(&data);
        prop_assert_eq!(h.finalize(), reference(&data));
    }

    /// Splitting the input arbitrarily never changes the digest.
    #[test]
    fn streaming_split_invariant(
        data in proptest::collection::vec(any::<u8>(), 0..3000),
        cuts in proptest::collection::vec(any::<usize>(), 0..12),
    ) {
        let mut points: Vec<usize> = cuts.iter().map(|c| c % (data.len() + 1)).collect();
        points.sort_unstable();
        let mut h = QuickXorHash::new();
        let mut prev = 0;
        for p in points.into_iter().chain([data.len()]) {
            h.update(&data[prev..p]);
            prev = p;
        }
        prop_assert_eq!(h.finalize_base64(), quick_xor_hash_base64(&data));
    }
}
