use crate::model::{BlobRef, MAX_BLOB_BYTES};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use vibeface_session_delivery::Word;
use zeroize::Zeroizing;

const EXTENT_BYTES: usize = 8 * 1024 * 1024;
const PREFIX_BYTES: usize = 32 + 32 + 8;

fn blob_hash(blob: &BlobRef) -> Result<[u8; 32]> {
    ensure!(blob.hash.len() == 64, "invalid blob hash length");
    ensure!(
        blob.hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid blob hash"
    );
    let mut hash = [0; 32];
    hex::decode_to_slice(&blob.hash, &mut hash)?;
    Ok(hash)
}

fn checked_size(blob: &BlobRef) -> Result<usize> {
    ensure!(
        blob.size <= MAX_BLOB_BYTES as u64,
        "attachment exceeds the 64 MiB limit"
    );
    Ok(blob.size as usize)
}

fn expected_count(blob: &BlobRef) -> Result<usize> {
    let size = checked_size(blob)?;
    blob_hash(blob)?;
    Ok(if size == 0 {
        1
    } else {
        size.div_ceil(EXTENT_BYTES)
    })
}

pub(super) fn check_count(blob: &BlobRef, count: usize) -> Result<()> {
    ensure!(
        count == expected_count(blob)?,
        "incorrect attachment part count"
    );
    Ok(())
}

pub(super) fn part_size(blob: &BlobRef, index: usize) -> Result<usize> {
    let count = expected_count(blob)?;
    ensure!(index < count, "attachment part index out of range");
    let size = blob.size as usize;
    let offset = index * EXTENT_BYTES;
    Ok(PREFIX_BYTES + size.saturating_sub(offset).min(EXTENT_BYTES))
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(super) fn encode_parts(
    event_hash: &Word,
    blob: &BlobRef,
    bytes: &[u8],
) -> Result<Vec<Zeroizing<Vec<u8>>>> {
    let file_hash = blob_hash(blob)?;
    let size = checked_size(blob)?;
    ensure!(
        bytes.len() == size,
        "attachment size does not match its reference"
    );
    ensure!(
        digest(bytes) == file_hash,
        "attachment hash does not match its reference"
    );

    let count = expected_count(blob)?;
    let mut parts = Vec::with_capacity(count);
    for index in 0..count {
        let offset = index * EXTENT_BYTES;
        let end = (offset + EXTENT_BYTES).min(bytes.len());
        let mut part = Zeroizing::new(Vec::with_capacity(part_size(blob, index)?));
        part.extend_from_slice(event_hash);
        part.extend_from_slice(&file_hash);
        part.extend_from_slice(&(offset as u64).to_be_bytes());
        part.extend_from_slice(&bytes[offset..end]);
        parts.push(part);
    }
    Ok(parts)
}

pub(super) fn assemble(
    event_hash: &Word,
    blob: &BlobRef,
    parts: &[Zeroizing<Vec<u8>>],
) -> Result<Zeroizing<Vec<u8>>> {
    let file_hash = blob_hash(blob)?;
    let size = checked_size(blob)?;
    check_count(blob, parts.len())?;

    let mut bytes = Zeroizing::new(Vec::with_capacity(size));
    for (index, part) in parts.iter().enumerate() {
        ensure!(
            part.len() == part_size(blob, index)?,
            "incorrect attachment part size"
        );
        ensure!(
            part[..32] == event_hash[..],
            "attachment event hash mismatch"
        );
        ensure!(part[32..64] == file_hash, "attachment blob hash mismatch");
        let offset = (index * EXTENT_BYTES) as u64;
        ensure!(
            part[64..72] == offset.to_be_bytes(),
            "attachment part offset mismatch"
        );
        bytes.extend_from_slice(&part[PREFIX_BYTES..]);
    }
    ensure!(bytes.len() == size, "assembled attachment size mismatch");
    ensure!(
        digest(&bytes) == file_hash,
        "assembled attachment hash mismatch"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(bytes: &[u8]) -> BlobRef {
        BlobRef {
            hash: hex::encode(digest(bytes)),
            size: bytes.len() as u64,
        }
    }

    #[test]
    fn round_trip_crosses_extent_boundary() {
        let bytes = vec![0x5a; EXTENT_BYTES + 1];
        let blob = blob(&bytes);
        let event_hash = [7; 32];
        let parts = encode_parts(&event_hash, &blob, &bytes).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), PREFIX_BYTES + EXTENT_BYTES);
        assert_eq!(parts[1].len(), PREFIX_BYTES + 1);
        assert_eq!(&*assemble(&event_hash, &blob, &parts).unwrap(), &bytes);
    }

    #[test]
    fn empty_file_has_one_empty_extent() {
        let blob = blob(&[]);
        let event_hash = [3; 32];
        let parts = encode_parts(&event_hash, &blob, &[]).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), PREFIX_BYTES);
        assert!(assemble(&event_hash, &blob, &parts).unwrap().is_empty());
    }

    #[test]
    fn rejects_wrong_event_and_corrupt_bytes() {
        let bytes = b"attachment";
        let blob = blob(bytes);
        let event_hash = [1; 32];
        let mut parts = encode_parts(&event_hash, &blob, bytes).unwrap();
        assert!(assemble(&[2; 32], &blob, &parts).is_err());
        parts[0][PREFIX_BYTES] ^= 1;
        assert!(assemble(&event_hash, &blob, &parts).is_err());
    }

    #[test]
    fn rejects_missing_or_duplicate_parts() {
        let bytes = vec![0x2a; EXTENT_BYTES + 1];
        let blob = blob(&bytes);
        let event_hash = [9; 32];
        let parts = encode_parts(&event_hash, &blob, &bytes).unwrap();
        assert!(assemble(&event_hash, &blob, &parts[..1]).is_err());
        assert!(assemble(&event_hash, &blob, &[parts[0].clone(), parts[0].clone()]).is_err());
    }
}
