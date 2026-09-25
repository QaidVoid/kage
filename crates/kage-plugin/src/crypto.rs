//! `kage.crypto`: synchronous cryptographic primitives for plugins.
//!
//! Stateless pure functions over byte strings: random bytes, SHA-2
//! digests, HMAC-SHA256, HKDF-SHA256, AES-256-GCM decrypt, Ed25519
//! signing, and base64 and hex conversion. Lua strings cross the
//! boundary as raw bytes, so digests, keys, and ciphertexts never
//! need a text encoding. Errors are generic and never echo their
//! inputs. All work runs on the host, so it never consumes the Lua
//! instruction budget the way a pure Lua loop would.

use mlua::{Lua, Table};

use kage_core::sync::lock;

use crate::capabilities::{Capability, CapabilityRegistry};
use crate::error::PluginError;

/// Most random bytes one call returns. Bounds a typo-driven allocation.
const MAX_RANDOM_BYTES: usize = 1_048_576;

/// Most bytes HKDF-SHA256 emits per call: 255 times the 32 byte digest.
const MAX_HKDF_BYTES: usize = 8_160;

/// Expected PKCS#8 DER prefix of an Ed25519 private key: SEQUENCE,
/// version 0, Ed25519 OID, OCTET STRING of 34 bytes holding the
/// 32 byte seed.
const ED25519_PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Install the base `kage.crypto` placeholder table.
///
/// The primitives themselves are gated behind the `crypto`
/// capability and attached per-plugin by `register`. The empty base
/// table keeps `kage.crypto` resolvable so an ungranted plugin sees
/// `kage.crypto.sha256` as `nil` rather than indexing a nil value.
pub fn install_crypto(lua: &Lua) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set("crypto", lua.create_table()?)?;
    Ok(())
}

/// Register the `crypto` capability installer.
///
/// Run (via `request_capabilities`) against a granted plugin's `kage`
/// proxy, it shadows the empty base `kage.crypto` with a table
/// carrying the primitives, so only a plugin the user granted
/// `crypto` can call them.
pub(crate) fn register(registry: &CapabilityRegistry) {
    let mut reg = lock(registry);
    reg.insert(
        Capability::Crypto,
        Box::new(|lua: &Lua, pkage: &Table| {
            pkage.set("crypto", build_crypto_table(lua)?)?;
            Ok(())
        }),
    );
}

/// Build the populated `kage.crypto` table. The helpers are
/// stateless, so the same table shape is attached onto each granted
/// plugin's proxy.
fn build_crypto_table(lua: &Lua) -> mlua::Result<Table> {
    let crypto = lua.create_table()?;
    add_hashes(lua, &crypto)?;
    add_kdf(lua, &crypto)?;
    add_aead_sign(lua, &crypto)?;
    add_codecs(lua, &crypto)?;
    Ok(crypto)
}

/// Hashes and random bytes on the shared table.
fn add_hashes(lua: &Lua, crypto: &Table) -> mlua::Result<()> {
    crypto.set(
        "random_bytes",
        lua.create_function(|lua, count: i64| {
            let count = positive_usize(count, "random_bytes", MAX_RANDOM_BYTES)?;
            let mut buf = vec![0u8; count];
            getrandom::fill(&mut buf)
                .map_err(|e| mlua::Error::external(format!("crypto.random_bytes: {e}")))?;
            lua.create_string(&buf)
        })?,
    )?;

    crypto.set(
        "sha256",
        lua.create_function(|lua, data: mlua::String| {
            use sha2::Digest as _;
            let data: Vec<u8> = data.as_bytes().to_vec();
            let digest = sha2::Sha256::digest(&data);
            lua.create_string(digest.as_slice())
        })?,
    )?;

    crypto.set(
        "sha512",
        lua.create_function(|lua, data: mlua::String| {
            use sha2::Digest as _;
            let data: Vec<u8> = data.as_bytes().to_vec();
            let digest = sha2::Sha512::digest(&data);
            lua.create_string(digest.as_slice())
        })?,
    )?;
    Ok(())
}

/// Key derivation on the shared table.
fn add_kdf(lua: &Lua, crypto: &Table) -> mlua::Result<()> {
    crypto.set(
        "hmac_sha256",
        lua.create_function(|lua, (key, data): (mlua::String, mlua::String)| {
            use hmac::Mac as _;
            let key: Vec<u8> = key.as_bytes().to_vec();
            let data: Vec<u8> = data.as_bytes().to_vec();
            let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key)
                .map_err(|_| mlua::Error::external("crypto.hmac_sha256: invalid key"))?;
            mac.update(&data);
            let out = mac.finalize().into_bytes();
            lua.create_string(out.as_slice())
        })?,
    )?;

    crypto.set(
        "hkdf_sha256",
        lua.create_function(
            |lua, (ikm, salt, info, length): (mlua::String, mlua::String, mlua::String, i64)| {
                let length = positive_usize(length, "hkdf_sha256", MAX_HKDF_BYTES)?;
                let ikm: Vec<u8> = ikm.as_bytes().to_vec();
                let salt: Vec<u8> = salt.as_bytes().to_vec();
                let info: Vec<u8> = info.as_bytes().to_vec();
                let salt_opt = if salt.is_empty() {
                    None
                } else {
                    Some(&salt[..])
                };
                let hkdf = hkdf::Hkdf::<sha2::Sha256>::new(salt_opt, &ikm);
                let mut out = vec![0u8; length];
                hkdf.expand(&info, &mut out)
                    .map_err(|_| mlua::Error::external("crypto.hkdf_sha256: expand failed"))?;
                lua.create_string(&out)
            },
        )?,
    )?;
    Ok(())
}

/// Authenticated decryption and signing on the shared table.
fn add_aead_sign(lua: &Lua, crypto: &Table) -> mlua::Result<()> {
    crypto.set(
        "aes256gcm_decrypt",
        lua.create_function(
            |lua,
             (key, iv, aad, ciphertext, tag): (
                mlua::String,
                mlua::String,
                mlua::String,
                mlua::String,
                mlua::String,
            )| {
                use aes_gcm::aead::{Aead as _, KeyInit as _, Payload};
                let key: Vec<u8> = key.as_bytes().to_vec();
                let iv: Vec<u8> = iv.as_bytes().to_vec();
                let aad: Vec<u8> = aad.as_bytes().to_vec();
                let ciphertext: Vec<u8> = ciphertext.as_bytes().to_vec();
                let tag: Vec<u8> = tag.as_bytes().to_vec();
                if key.len() != 32 {
                    return Err(mlua::Error::external(
                        "crypto.aes256gcm_decrypt: key must be 32 bytes",
                    ));
                }
                if iv.len() != 12 {
                    return Err(mlua::Error::external(
                        "crypto.aes256gcm_decrypt: iv must be 12 bytes",
                    ));
                }
                let cipher =
                    aes_gcm::Aes256Gcm::new(aes_gcm::Key::<aes_gcm::Aes256Gcm>::from_slice(&key));
                let mut sealed = ciphertext;
                sealed.extend_from_slice(&tag);
                let plain = cipher
                    .decrypt(
                        aes_gcm::Nonce::from_slice(&iv),
                        Payload {
                            msg: &sealed,
                            aad: &aad,
                        },
                    )
                    .map_err(|_| mlua::Error::external("crypto.aes256gcm_decrypt: failed"))?;
                lua.create_string(&plain)
            },
        )?,
    )?;

    crypto.set(
        "ed25519_sign",
        lua.create_function(
            |lua, (private_key_pkcs8, message): (mlua::String, mlua::String)| {
                use ed25519_dalek::Signer as _;
                let private_key_pkcs8: Vec<u8> = private_key_pkcs8.as_bytes().to_vec();
                let message: Vec<u8> = message.as_bytes().to_vec();
                let seed = ed25519_seed(&private_key_pkcs8)?;
                let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
                let signature = signing.sign(&message);
                lua.create_string(signature.to_bytes().as_slice())
            },
        )?,
    )?;
    Ok(())
}

/// Text codecs on the shared table.
fn add_codecs(lua: &Lua, crypto: &Table) -> mlua::Result<()> {
    crypto.set(
        "to_base64",
        lua.create_function(|lua, data: mlua::String| {
            use base64::Engine as _;
            let data: Vec<u8> = data.as_bytes().to_vec();
            lua.create_string(
                base64::engine::general_purpose::STANDARD
                    .encode(&data)
                    .as_bytes(),
            )
        })?,
    )?;

    crypto.set(
        "from_base64",
        lua.create_function(|lua, text: mlua::String| {
            use base64::Engine as _;
            let text: Vec<u8> = text.as_bytes().to_vec();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&text)
                .map_err(|_| mlua::Error::external("crypto.from_base64: invalid base64"))?;
            lua.create_string(&bytes)
        })?,
    )?;

    crypto.set(
        "to_hex",
        lua.create_function(|lua, data: mlua::String| {
            let data: Vec<u8> = data.as_bytes().to_vec();
            lua.create_string(hex::encode(&data).as_bytes())
        })?,
    )?;

    crypto.set(
        "from_hex",
        lua.create_function(|lua, text: mlua::String| {
            let text: Vec<u8> = text.as_bytes().to_vec();
            let bytes = hex::decode(&text)
                .map_err(|_| mlua::Error::external("crypto.from_hex: invalid hex"))?;
            lua.create_string(&bytes)
        })?,
    )?;
    Ok(())
}

/// Read the 32 byte seed out of a PKCS#8 DER Ed25519 private key.
/// Anything else errors without echoing the input.
fn ed25519_seed(der: &[u8]) -> mlua::Result<[u8; 32]> {
    if der.len() != ED25519_PKCS8_PREFIX.len() + 32
        || &der[..ED25519_PKCS8_PREFIX.len()] != ED25519_PKCS8_PREFIX
    {
        return Err(mlua::Error::external(
            "crypto.ed25519_sign: invalid private key",
        ));
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&der[ED25519_PKCS8_PREFIX.len()..]);
    Ok(seed)
}

/// Validate a Lua integer count as a `usize` in `1..=max`.
fn positive_usize(count: i64, caller: &str, max: usize) -> mlua::Result<usize> {
    let count = usize::try_from(count)
        .map_err(|_| mlua::Error::external(format!("{caller}: count out of range")))?;
    if count == 0 || count > max {
        return Err(mlua::Error::external(format!(
            "{caller}: count out of range"
        )));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    /// A runtime that grants `crypto` to plugin `p`.
    fn rt_with_crypto() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("p".to_owned(), vec!["crypto".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    /// Evaluate `script` as the granted plugin `p` after requesting
    /// the `crypto` capability, so `kage.crypto` is attached.
    fn eval_crypto(script: &str) -> String {
        let rt = rt_with_crypto();
        let script = format!("kage.request_capabilities({{'crypto'}}); {script}");
        let value = rt.eval_plugin("p", &script).unwrap();
        match value {
            mlua::Value::String(s) => String::from_utf8_lossy(&s.as_bytes()).into_owned(),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn ungranted_plugin_has_no_crypto() {
        let rt = rt_with_crypto();
        let v = rt
            .eval_plugin("other", "return kage.crypto.sha256 == nil")
            .unwrap();
        assert_eq!(v.as_boolean(), Some(true));
    }

    #[test]
    fn sha256_matches_the_standard_vector() {
        let hex = eval_crypto("return kage.crypto.to_hex(kage.crypto.sha256('abc'))");
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha512_matches_the_standard_vector() {
        let hex = eval_crypto("return kage.crypto.to_hex(kage.crypto.sha512('abc'))");
        assert_eq!(
            hex,
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_case_1() {
        let key = "kage.crypto.from_hex('0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b')";
        let hex = eval_crypto(&format!(
            "return kage.crypto.to_hex(kage.crypto.hmac_sha256({key}, 'Hi There'))"
        ));
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hkdf_sha256_matches_both_reference_vectors() {
        let secret = "'test-secret-not-a-credential'";
        let salt = "'WD_CLIENT_SIGN_KDF_SALT'";
        let first = eval_crypto(&format!(
            "return kage.crypto.to_hex(kage.crypto.hkdf_sha256({secret}, {salt}, 'getSignKey_hmac', 32))",
        ));
        assert_eq!(
            first,
            "24cc9a2ce9f174111e4909f782f44b085d1c31dd49479b2fa0da353bfa24029e"
        );
        let second = eval_crypto(&format!(
            "return kage.crypto.to_hex(kage.crypto.hkdf_sha256({secret}, {salt}, 'ed25519_priv', 32))",
        ));
        assert_eq!(
            second,
            "fbbcca63d7bdbbe3cff7bc59a0c8d3d137ee7bb9c3fbd928579d49a686849727"
        );
    }

    #[test]
    fn handshake_mac_matches_the_reference_vector() {
        let script = concat!(
            "local secret = 'test-secret-not-a-credential' ",
            "local key = kage.crypto.hkdf_sha256(secret, 'WD_CLIENT_SIGN_KDF_SALT', 'getSignKey_hmac', 32) ",
            "local mac = kage.crypto.hmac_sha256(key, 'get_sign_key\\n0123456789abcdef0123456789abcdef\\n1790090000000\\n00112233445566778899aabbccddeeff') ",
            "return kage.crypto.to_base64(mac)",
        );
        assert_eq!(
            eval_crypto(script),
            "P1FKsOgW3pzGsArShBCm1QA/w5WsdkoH/TqRa8Q6Bwg="
        );
    }

    #[test]
    fn aes256gcm_decrypt_opens_with_the_id_and_refuses_the_joined_value() {
        let cipher = "'AAECAwQFBgcICQoL6JXJnnq5J1DO8dk3HDLXrJwS2PkpjCXFSF9Ivr3nbyAZJcA5tWW5q7zFSH35+axhQ66o8ht/EGlruLcHcu8EtW1sUnsxRzondgPZV6XpEyI='";
        let secret = "'test-secret-not-a-credential'";
        let id = "'0123456789abcdef0123456789abcdef'";
        let script = format!(
            concat!(
                "local sealed = kage.crypto.from_base64({cipher}) ",
                "local iv = sealed:sub(1, 12) ",
                "local rest = sealed:sub(13) ",
                "local ct = rest:sub(1, #rest - 16) ",
                "local tag = rest:sub(#rest - 15) ",
                "local akey = kage.crypto.hkdf_sha256({secret}, 'WD_CLIENT_SIGN_KDF_SALT', 'ed25519_priv', 32) ",
                "local plain = kage.crypto.aes256gcm_decrypt(akey, iv, {id}, ct, tag) ",
                "return tostring(#kage.crypto.from_base64(plain))",
            ),
            cipher = cipher,
            secret = secret,
            id = id,
        );
        assert_eq!(eval_crypto(&script), "48");

        let joined = "'0123456789abcdef0123456789abcdef.test-secret-not-a-credential'";
        let bad = format!(
            concat!(
                "local sealed = kage.crypto.from_base64({cipher}) ",
                "local iv = sealed:sub(1, 12) ",
                "local rest = sealed:sub(13) ",
                "local ct = rest:sub(1, #rest - 16) ",
                "local tag = rest:sub(#rest - 15) ",
                "local akey = kage.crypto.hkdf_sha256({secret}, 'WD_CLIENT_SIGN_KDF_SALT', 'ed25519_priv', 32) ",
                "local ok, _ = pcall(kage.crypto.aes256gcm_decrypt, akey, iv, {joined}, ct, tag) ",
                "return ok and 'opened' or 'refused'",
            ),
            cipher = cipher,
            secret = secret,
            joined = joined,
        );
        assert_eq!(eval_crypto(&bad), "refused");
    }

    #[test]
    fn ed25519_sign_matches_the_reference_signature() {
        let script = concat!(
            "local cipher = 'AAECAwQFBgcICQoL6JXJnnq5J1DO8dk3HDLXrJwS2PkpjCXFSF9Ivr3nbyAZJcA5tWW5q7zFSH35+axhQ66o8ht/EGlruLcHcu8EtW1sUnsxRzondgPZV6XpEyI=' ",
            "local id = '0123456789abcdef0123456789abcdef' ",
            "local secret = 'test-secret-not-a-credential' ",
            "local sealed = kage.crypto.from_base64(cipher) ",
            "local iv = sealed:sub(1, 12) ",
            "local rest = sealed:sub(13) ",
            "local ct = rest:sub(1, #rest - 16) ",
            "local tag = rest:sub(#rest - 15) ",
            "local akey = kage.crypto.hkdf_sha256(secret, 'WD_CLIENT_SIGN_KDF_SALT', 'ed25519_priv', 32) ",
            "local plain = kage.crypto.aes256gcm_decrypt(akey, iv, id, ct, tag) ",
            "local pkcs8 = kage.crypto.from_base64(plain) ",
            "local msg = id .. '\\n1790090000000\\n3.12.3\\n7c9e6679-7425-40de-944b-e07fc1f90ae7\\n00112233445566778899aabbccddeeff' ",
            "return kage.crypto.to_base64(kage.crypto.ed25519_sign(pkcs8, msg))",
        );
        assert_eq!(
            eval_crypto(script),
            "opXMkTWwZ4qSyxge5kaDJa+Yf3m7VhaMZyxWxHSKVNsXcnZiwfhH3XiVVUl9BnSxisreDmVXVZVGIm9hFtXWAw=="
        );
    }

    #[test]
    fn ed25519_sign_rejects_a_malformed_key() {
        let rt = rt_with_crypto();
        let res = rt.eval_plugin(
            "p",
            "kage.request_capabilities({'crypto'}); return kage.crypto.ed25519_sign('short', 'msg')",
        );
        assert!(res.is_err());
    }

    #[test]
    fn base64_and_hex_round_trip() {
        assert_eq!(eval_crypto("return kage.crypto.to_hex('abc')"), "616263");
        assert_eq!(
            eval_crypto("return kage.crypto.from_hex(kage.crypto.to_hex('abc'))"),
            "abc"
        );
        assert_eq!(eval_crypto("return kage.crypto.to_base64('abc')"), "YWJj");
        assert_eq!(
            eval_crypto("return kage.crypto.from_base64(kage.crypto.to_base64('abc'))"),
            "abc"
        );
    }

    #[test]
    fn from_hex_and_from_base64_reject_garbage() {
        let rt = rt_with_crypto();
        for script in [
            "kage.request_capabilities({'crypto'}); return kage.crypto.from_hex('zz')",
            "kage.request_capabilities({'crypto'}); return kage.crypto.from_base64('!!!')",
            "kage.request_capabilities({'crypto'}); return kage.crypto.random_bytes(0)",
            "kage.request_capabilities({'crypto'}); return kage.crypto.hkdf_sha256('k', 's', 'i', 9000)",
        ] {
            assert!(rt.eval_plugin("p", script).is_err(), "{script}");
        }
    }

    #[test]
    fn random_bytes_returns_the_asked_length() {
        let len = eval_crypto("return tostring(#kage.crypto.random_bytes(16))");
        assert_eq!(len, "16");
        let pair = eval_crypto(
            "local a = kage.crypto.random_bytes(32) local b = kage.crypto.random_bytes(32) return (a == b) and 'same' or 'diff'",
        );
        assert_eq!(pair, "diff");
    }
}
