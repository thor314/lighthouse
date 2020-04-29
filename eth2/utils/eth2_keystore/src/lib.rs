mod checksum;
mod cipher;
mod crypto;
mod kdf;

use crate::cipher::Cipher;
use crate::crypto::Crypto;
use crate::kdf::Kdf;
use bls::{Keypair, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use serde_repr::*;
use uuid::Uuid;

pub use crate::crypto::Password;

/// Version for `Keystore`.
#[derive(Debug, Clone, PartialEq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum Version {
    V4 = 4,
}

impl Default for Version {
    fn default() -> Self {
        Version::V4
    }
}

/// TODO: Implement `path` according to
/// https://github.com/CarlBeek/EIPs/blob/bls_path/EIPS/eip-2334.md
/// For now, `path` is set to en empty string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keystore {
    pub crypto: Crypto,
    pub uuid: Uuid,
    pub path: String,
    pub pubkey: String,
    pub version: Version,
}

impl Keystore {
    /// Generate `Keystore` object for a BLS12-381 secret key from a
    /// keypair and password. Optionally, provide params for kdf, cipher and a uuid.
    pub fn new(
        keypair: &Keypair,
        password: Password,
        kdf: Option<Kdf>,
        cipher: Option<Cipher>,
        uuid: Option<Uuid>,
    ) -> Self {
        let crypto = Crypto::encrypt(
            password,
            &keypair.sk.as_raw().as_bytes(),
            kdf.unwrap_or_default(),
            cipher.unwrap_or_default(),
        );
        let uuid = uuid.unwrap_or(Uuid::new_v4());
        let version = Version::default();
        let path = String::new();
        Keystore {
            crypto,
            uuid,
            path,
            pubkey: keypair.pk.as_hex_string()[2..].to_string(),
            version,
        }
    }

    /// Regenerate a BLS12-381 `Keypair` from given the `Keystore` object and
    /// the correct password.
    ///
    /// An error is returned if the password provided is incorrect or if
    /// keystore does not contain valid hex strings or if the secret contained is not a
    /// BLS12-381 secret key.
    pub fn to_keypair(&self, password: Password) -> Result<Keypair, String> {
        let sk_bytes = self.crypto.decrypt(password)?;
        if sk_bytes.len() != 32 {
            return Err(format!("Invalid secret key size: {:?}", sk_bytes));
        }
        let sk = SecretKey::from_bytes(sk_bytes.as_ref())
            .map_err(|e| format!("Invalid secret key in keystore {:?}", e))?;
        let pk = PublicKey::from_secret_key(&sk);
        if pk.as_hex_string()[2..].to_string() != self.pubkey {
            return Err(format!("Decoded pubkey doesn't match keystore pubkey"));
        }
        Ok(Keypair { sk, pk })
    }
}

// Test cases taken from https://github.com/CarlBeek/EIPs/blob/bls_keystore/EIPS/eip-2335.md#test-cases
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vectors() {
        let expected_secret = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
        let password: Password = "testpassword".into();
        let scrypt_test_vector = r#"
            {
            "crypto": {
                "kdf": {
                    "function": "scrypt",
                    "params": {
                        "dklen": 32,
                        "n": 262144,
                        "p": 1,
                        "r": 8,
                        "salt": "d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"
                    },
                    "message": ""
                },
                "checksum": {
                    "function": "sha256",
                    "params": {},
                    "message": "149aafa27b041f3523c53d7acba1905fa6b1c90f9fef137568101f44b531a3cb"
                },
                "cipher": {
                    "function": "aes-128-ctr",
                    "params": {
                        "iv": "264daa3f303d7259501c93d997d84fe6"
                    },
                    "message": "54ecc8863c0550351eee5720f3be6a5d4a016025aa91cd6436cfec938d6a8d30"
                }
            },
            "pubkey": "9612d7a727c9d0a22e185a1c768478dfe919cada9266988cb32359c11f2b7b27f4ae4040902382ae2910c15e2b420d07",
            "uuid": "1d85ae20-35c5-4611-98e8-aa14a633906f",
            "path": "",
            "version": 4
        }
        "#;

        let pbkdf2_test_vector = r#"
            {
            "crypto": {
                "kdf": {
                    "function": "pbkdf2",
                    "params": {
                        "dklen": 32,
                        "c": 262144,
                        "prf": "hmac-sha256",
                        "salt": "d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"
                    },
                    "message": ""
                },
                "checksum": {
                    "function": "sha256",
                    "params": {},
                    "message": "18b148af8e52920318084560fd766f9d09587b4915258dec0676cba5b0da09d8"
                },
                "cipher": {
                    "function": "aes-128-ctr",
                    "params": {
                        "iv": "264daa3f303d7259501c93d997d84fe6"
                    },
                    "message": "a9249e0ca7315836356e4c7440361ff22b9fe71e2e2ed34fc1eb03976924ed48"
                }
            },
            "pubkey": "9612d7a727c9d0a22e185a1c768478dfe919cada9266988cb32359c11f2b7b27f4ae4040902382ae2910c15e2b420d07",
            "path": "m/12381/60/0/0",
            "uuid": "64625def-3331-4eea-ab6f-782f3ed16a83",
            "version": 4
        }
        "#;
        let test_vectors = vec![scrypt_test_vector, pbkdf2_test_vector];
        for test in test_vectors {
            let keystore: Keystore = serde_json::from_str(test).unwrap();
            let keypair = keystore.to_keypair(password.clone()).unwrap();
            let expected_sk = hex::decode(expected_secret).unwrap();
            assert_eq!(keypair.sk.as_raw().as_bytes(), expected_sk)
        }
    }
}