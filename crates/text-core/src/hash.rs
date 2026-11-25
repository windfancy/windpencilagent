//! 哈希：md5 / sha1 / sha256 / sha512（用于完整性比对、指纹、口令格式识别）。
//!
//! 说明：md5/sha1 只用于**识别与比对**（很多设备指纹只给 md5），不用于任何安全承诺。

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

pub const ALGOS: &[&str] = &["md5", "sha1", "sha256", "sha512"];

pub fn hash_bytes(algo: &str, data: &[u8]) -> Result<String, String> {
    let hex = match algo.trim().to_ascii_lowercase().as_str() {
        "md5" => format!("{:x}", Md5::digest(data)),
        "sha1" => format!("{:x}", Sha1::digest(data)),
        "sha256" => format!("{:x}", Sha256::digest(data)),
        "sha512" => format!("{:x}", Sha512::digest(data)),
        other => {
            return Err(format!("不支持的哈希算法：{other}（可选 {}）", ALGOS.join("/")));
        }
    };
    Ok(hex)
}

/// 一次算全部算法（报告里常要多种指纹）。
pub fn hash_all(data: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "md5": hash_bytes("md5", data).unwrap_or_default(),
        "sha1": hash_bytes("sha1", data).unwrap_or_default(),
        "sha256": hash_bytes("sha256", data).unwrap_or_default(),
        "sha512": hash_bytes("sha512", data).unwrap_or_default(),
        "size": data.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(hash_bytes("md5", b"abc").unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hash_bytes("sha1", b"abc").unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hash_bytes("sha256", b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rejects_unknown() {
        assert!(hash_bytes("crc32", b"x").is_err());
    }

    #[test]
    fn hash_all_has_all_keys() {
        let v = hash_all(b"hi");
        for k in ["md5", "sha1", "sha256", "sha512"] {
            assert!(v[k].as_str().map(|s| !s.is_empty()).unwrap_or(false), "{k} 缺失");
        }
        assert_eq!(v["size"], 2);
    }
}
