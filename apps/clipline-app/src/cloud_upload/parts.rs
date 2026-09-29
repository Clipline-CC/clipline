//! Upload part preparation, slicing, hashing, and validation.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileSlice {
    offset: u64,
    pub(crate) length: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PreparedUploadPart {
    pub(crate) slice: FileSlice,
    pub(crate) checksum_sha256: String,
}

pub(crate) async fn prepare_upload_part(
    path: &Path,
    file_size: u64,
    part_size_bytes: u64,
    part_number: u16,
) -> CloudApiResult<PreparedUploadPart> {
    let slice = part_slice_for(file_size, part_size_bytes, part_number)?;
    let checksum_sha256 = sha256_file_slice(path, slice).await?;
    Ok(PreparedUploadPart {
        slice,
        checksum_sha256,
    })
}

pub(crate) fn part_slice_for(
    file_size: u64,
    part_size_bytes: u64,
    part_number: u16,
) -> CloudApiResult<FileSlice> {
    if part_size_bytes == 0 {
        return Err(CloudApiError::InvalidUpload(
            "part size must be positive".to_string(),
        ));
    }
    if part_size_bytes > MAX_UPLOAD_PART_BYTES {
        return Err(CloudApiError::InvalidUpload(format!(
            "server part size {part_size_bytes} exceeds the {} byte client limit",
            MAX_UPLOAD_PART_BYTES
        )));
    }
    if part_number == 0 {
        return Err(CloudApiError::InvalidUpload(
            "part numbers start at 1".to_string(),
        ));
    }
    let index = u64::from(part_number - 1);
    let start = index
        .checked_mul(part_size_bytes)
        .ok_or_else(|| CloudApiError::InvalidUpload("part offset overflowed".to_string()))?;
    if start >= file_size {
        return Err(CloudApiError::InvalidUpload(format!(
            "part {part_number} starts beyond the upload file"
        )));
    }
    Ok(FileSlice {
        offset: start,
        length: part_size_bytes.min(file_size - start),
    })
}

pub(crate) async fn open_part_reader(
    path: &Path,
    slice: FileSlice,
) -> CloudApiResult<tokio::io::Take<tokio::fs::File>> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| upload_file_error("open upload part", path, error))?;
    file.seek(std::io::SeekFrom::Start(slice.offset))
        .await
        .map_err(|error| upload_file_error("seek upload part", path, error))?;
    Ok(file.take(slice.length))
}

pub(crate) async fn sha256_file_slice(path: &Path, slice: FileSlice) -> CloudApiResult<String> {
    let mut reader = open_part_reader(path, slice).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total_read = 0_u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| upload_file_error("hash upload part", path, error))?;
        if read == 0 {
            break;
        }
        total_read += read as u64;
        hasher.update(&buffer[..read]);
    }
    if total_read != slice.length {
        return Err(upload_file_error(
            "hash upload part",
            path,
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "expected {} bytes at offset {}, found {total_read}",
                    slice.length, slice.offset
                ),
            ),
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) async fn part_request_body(path: &Path, slice: FileSlice) -> CloudApiResult<reqwest::Body> {
    let reader = open_part_reader(path, slice).await?;
    Ok(reqwest::Body::wrap_stream(ReaderStream::new(reader)))
}

pub(crate) fn validate_missing_parts(
    missing_parts: &[u16],
    file_size: u64,
    part_size_bytes: u64,
) -> CloudApiResult<()> {
    if part_size_bytes == 0 {
        return Err(CloudApiError::InvalidUpload(
            "part size must be positive".to_string(),
        ));
    }
    if part_size_bytes > MAX_UPLOAD_PART_BYTES {
        return Err(CloudApiError::InvalidUpload(format!(
            "server part size {part_size_bytes} exceeds the {} byte client limit",
            MAX_UPLOAD_PART_BYTES
        )));
    }

    let total_parts = file_size.div_ceil(part_size_bytes);
    if total_parts > u64::from(u16::MAX) {
        return Err(CloudApiError::InvalidUpload(format!(
            "upload requires {total_parts} parts, exceeding the protocol limit"
        )));
    }

    let mut seen = BTreeSet::new();
    for &part_number in missing_parts {
        if part_number == 0 {
            return Err(CloudApiError::InvalidUpload(
                "part numbers start at 1".to_string(),
            ));
        }
        if u64::from(part_number) > total_parts {
            return Err(CloudApiError::InvalidUpload(format!(
                "part {part_number} starts beyond the upload file"
            )));
        }
        if !seen.insert(part_number) {
            return Err(CloudApiError::InvalidUpload(format!(
                "server returned duplicate part {part_number}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn upload_file_error(action: &str, path: &Path, error: std::io::Error) -> CloudApiError {
    CloudApiError::InvalidUpload(format!("{action} {path:?}: {error}"))
}

pub(crate) async fn sha256_file(path: &Path) -> CloudApiResult<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| upload_file_error("open upload for hashing", path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| upload_file_error("hash upload", path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_cloud_api::sha256_hex;
    

    #[tokio::test]
    async fn file_hash_and_part_streams_are_retryable_and_bounded() {
        let path =
            std::env::temp_dir().join(format!("clipline-upload-source-{}.bin", std::process::id()));
        tokio::fs::write(&path, b"abcdef").await.unwrap();

        assert_eq!(sha256_file(&path).await.unwrap(), sha256_hex(b"abcdef"));
        let part = prepare_upload_part(&path, 6, 3, 2).await.unwrap();
        assert_eq!(part.slice.offset, 3);
        assert_eq!(part.slice.length, 3);
        assert_eq!(part.checksum_sha256, sha256_hex(b"def"));

        let mut first_attempt = open_part_reader(&path, part.slice).await.unwrap();
        let mut first_bytes = Vec::new();
        first_attempt.read_to_end(&mut first_bytes).await.unwrap();
        assert_eq!(first_bytes, b"def");

        let mut retry_attempt = open_part_reader(&path, part.slice).await.unwrap();
        let mut retry_bytes = Vec::new();
        retry_attempt.read_to_end(&mut retry_bytes).await.unwrap();
        assert_eq!(retry_bytes, b"def");
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn hostile_part_size_is_rejected_before_file_allocation() {
        let missing = Path::new("this-file-must-not-be-opened.mp4");
        let error = prepare_upload_part(missing, u64::MAX, MAX_UPLOAD_PART_BYTES + 1, 1)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn multipart_work_list_rejects_zero_duplicates_and_out_of_range_parts() {
        let zero = validate_missing_parts(&[0], 6, 3).unwrap_err();
        assert!(zero.to_string().contains("start at 1"), "{zero}");

        let duplicate = validate_missing_parts(&[1, 2, 1], 6, 3).unwrap_err();
        assert!(
            duplicate.to_string().contains("duplicate part 1"),
            "{duplicate}"
        );

        let out_of_range = validate_missing_parts(&[3], 6, 3).unwrap_err();
        assert!(
            out_of_range.to_string().contains("beyond"),
            "{out_of_range}"
        );
    }

    #[test]
    fn multipart_work_list_preserves_valid_resumable_subset() {
        validate_missing_parts(&[3, 1], 7, 3).unwrap();
        validate_missing_parts(&[], 7, 3).unwrap();
    }

}