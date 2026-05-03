use base64::Engine;
use byteorder::{LittleEndian, WriteBytesExt};
use num_bigint::BigUint;
use rand_core::OsRng;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use std::fs;
use std::path::PathBuf;

const RSA_KEY_SIZE: usize = 2048;

fn calculate_n0inv(n0: u32) -> u32 {
    let mut r = n0;
    let mut i = 0;
    while i < 4 {
        r = r.wrapping_mul(2u32.wrapping_sub(n0.wrapping_mul(r)));
        i += 1;
    }
    (!r).wrapping_add(1)
}

pub fn generate_adb_public_key(priv_key: &RsaPrivateKey) -> String {
    let n_bytes_be = priv_key.n().to_bytes_be();
    let n_biguint = BigUint::from_bytes_be(&n_bytes_be);

    let r = BigUint::from(1u32) << 2048;
    let r_squared: BigUint = (&r * &r) % &n_biguint;

    let n_bytes_le = n_biguint.to_bytes_le();
    let rr_bytes_le = r_squared.to_bytes_le();

    let mut n_256 = [0u8; 256];
    for (i, &b) in n_bytes_le.iter().enumerate().take(256) {
        n_256[i] = b;
    }

    let mut rr_256 = [0u8; 256];
    for (i, &b) in rr_bytes_le.iter().enumerate().take(256) {
        rr_256[i] = b;
    }

    let n0 = u32::from_le_bytes(n_256[0..4].try_into().unwrap());
    let n0inv = calculate_n0inv(n0);

    let mut buffer = Vec::with_capacity(524);
    buffer.write_u32::<LittleEndian>(64).unwrap();
    buffer.write_u32::<LittleEndian>(n0inv).unwrap();
    buffer.extend_from_slice(&n_256);
    buffer.extend_from_slice(&rr_256);
    buffer.write_u32::<LittleEndian>(65537).unwrap();

    let base64_str = base64::engine::general_purpose::STANDARD.encode(&buffer);
    format!("{} user@rustyadb\0", base64_str)
}

pub fn load_or_generate_private_key() -> RsaPrivateKey {
    let mut android_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    android_dir.push(".android");

    if !android_dir.exists() {
        if let Err(e) = fs::create_dir_all(&android_dir) {
            eprintln!("警告: 无法创建 .android 目录: {}", e);
        }
    }

    let priv_path = android_dir.join("adbkey");
    let pub_path = android_dir.join("adbkey.pub");

    if priv_path.exists() && pub_path.exists() {
        match fs::read_to_string(&priv_path) {
            Ok(content) => match RsaPrivateKey::from_pkcs8_pem(&content) {
                Ok(key) => return key,
                Err(e) => {
                    eprintln!("警告: 解析私钥失败: {}, 将重新生成", e);
                }
            },
            Err(e) => {
                eprintln!("警告: 读取私钥失败: {}, 将重新生成", e);
            }
        }
    }

    let mut rng = OsRng;
    let priv_key = loop {
        match RsaPrivateKey::new(&mut rng, RSA_KEY_SIZE) {
            Ok(key) => break key,
            Err(e) => {
                eprintln!("警告: 密钥生成失败: {}, 重试...", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };

    loop {
        match priv_key.to_pkcs8_pem(LineEnding::LF) {
            Ok(priv_pem) => {
                if let Err(e) = fs::write(&priv_path, priv_pem.as_bytes()) {
                    eprintln!("警告: 写入私钥失败: {}", e);
                } else {
                    break;
                }
            }
            Err(e) => {
                eprintln!("警告: 私钥格式化失败: {}", e);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let pub_key_str = generate_adb_public_key(&priv_key);

    loop {
        if let Err(e) = fs::write(&pub_path, pub_key_str.as_bytes()) {
            eprintln!("警告: 写入公钥失败: {}", e);
            std::thread::sleep(std::time::Duration::from_millis(100));
        } else {
            break;
        }
    }

    priv_key
}

pub fn sign_token(priv_pem: &str, token: &[u8]) -> Result<Vec<u8>, &'static str> {
    use rsa::pkcs8::DecodePrivateKey;

    let priv_key = RsaPrivateKey::from_pkcs8_pem(priv_pem).map_err(|_| "私钥解析失败")?;

    let mut digest_info = Vec::with_capacity(35);
    digest_info.extend_from_slice(&[
        0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
    ]);
    digest_info.extend_from_slice(token);

    let padding = rsa::pkcs1v15::Pkcs1v15Sign::new_raw();
    let signature = priv_key
        .sign(padding, &digest_info)
        .map_err(|_| "裸签计算失败")?;

    Ok(signature)
}

pub fn get_public_key() -> Vec<u8> {
    let mut android_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    android_dir.push(".android");
    let pub_path = android_dir.join("adbkey.pub");

    if !pub_path.exists() {
        load_or_generate_private_key();
    }

    loop {
        match fs::read(&pub_path) {
            Ok(content) => {
                let mut data = content;
                if !data.ends_with(&[0]) {
                    data.push(0);
                }
                return data;
            }
            Err(e) => {
                eprintln!("警告: 读取公钥失败: {}, 重试...", e);
                load_or_generate_private_key();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

pub fn get_or_create_keys() -> Result<(String, Vec<u8>), &'static str> {
    let priv_key = load_or_generate_private_key();

    let priv_pem = priv_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|_| "无法生成私钥 PEM")?
        .to_string();

    let pub_key = get_public_key();

    Ok((priv_pem, pub_key))
}
