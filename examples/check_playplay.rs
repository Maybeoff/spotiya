use std::{
    error::Error,
    fs,
    io::{Cursor, Read, Seek, SeekFrom},
    path::PathBuf,
};

use aes::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use librespot::audio::{AudioDecrypt, playplay::PlayPlayDecoder};
use librespot::core::audio_key::{AudioKey, AudioKeyMaterial};

type Cipher = ctr::Ctr128BE<aes::Aes128>;
const IV: [u8; 16] = [
    0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93,
];

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.len() != 4 {
        return Err("expected DECODER LICENSE_FILE ENCRYPTED_HEADER REFERENCE_KEY_FILE".into());
    }
    let config = PlayPlayDecoder::configuration(&args[0])?;
    let license = fs::read(&args[1])?;
    let key = config.parse_response(&license)?;
    let ciphertext = fs::read(&args[2])?;
    if ciphertext.is_empty() || ciphertext.len() > 24576 {
        return Err("invalid bounded sample size".into());
    }
    let reference_key: [u8; 16] = fs::read(&args[3])?
        .try_into()
        .map_err(|_| "invalid reference key size")?;
    let mut expected = ciphertext.clone();
    Cipher::new((&reference_key).into(), (&IV).into()).apply_keystream(&mut expected);
    let mut stream = AudioDecrypt::new_material(
        Some(AudioKeyMaterial::PlayPlay(key.clone())),
        Cursor::new(ciphertext.clone()),
    )?;
    let mut actual = vec![0; ciphertext.len()];
    stream.read_exact(&mut actual)?;
    if actual != expected {
        return Err("PlayPlay sample does not match the AP reference".into());
    }
    let offset = 167.min(ciphertext.len());
    stream.seek(SeekFrom::Start(offset as u64))?;
    let mut tail = Vec::new();
    stream.read_to_end(&mut tail)?;
    if tail != expected[offset..] {
        return Err("PlayPlay seek does not match the AP reference".into());
    }
    drop(stream);

    let mut legacy = AudioDecrypt::new(Some(AudioKey(reference_key)), Cursor::new(ciphertext));
    let mut legacy_bytes = Vec::new();
    legacy.read_to_end(&mut legacy_bytes)?;
    if legacy_bytes != expected {
        return Err("legacy AES decryption changed".into());
    }

    let mut split = AudioDecrypt::new_material(
        Some(AudioKeyMaterial::PlayPlay(key.clone())),
        Cursor::new(vec![0; 24593]),
    )?;
    let mut bytes = Vec::new();
    split.read_to_end(&mut bytes)?;
    let mut reference = vec![0; bytes.len()];
    Cipher::new((&reference_key).into(), (&IV).into()).apply_keystream(&mut reference);
    if bytes != reference {
        return Err("multi-frame read does not match the AP reference".into());
    }
    drop(split);

    let position = (1 << 20) - 31;
    let mut seek = AudioDecrypt::new_material(
        Some(AudioKeyMaterial::PlayPlay(key)),
        Cursor::new(vec![0; position + 512]),
    )?;
    seek.seek(SeekFrom::Start(position as u64))?;
    let mut ranged = vec![0; 512];
    seek.read_exact(&mut ranged)?;
    let mut reference = vec![0; 512];
    let mut cipher = Cipher::new((&reference_key).into(), (&IV).into());
    cipher.seek(position);
    cipher.apply_keystream(&mut reference);
    if ranged != reference {
        return Err("counter-boundary seek does not match the AP reference".into());
    }
    println!(
        "PlayPlay/AES sample, seek, multi-frame reads and counter boundary match; no playback started"
    );
    Ok(())
}
