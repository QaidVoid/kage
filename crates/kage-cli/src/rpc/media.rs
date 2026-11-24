//! Reads produced media from the authoritative session recording.

use base64::Engine as _;
use kage_acp::acp::{MediaReadRequest, MediaReadResponse};
use kage_core::Modality;
use kage_jsonrpc::RpcError;

/// The transport accepts 8 MiB frames; leave room for the JSON-RPC envelope.
const MEDIA_FRAME_BYTES: usize = 8 * 1024 * 1024 - 1024;

impl super::CliAcpAgent {
    /// Reads the newest produced media for a session this connection owns.
    pub(super) fn read_media(&self, req: &MediaReadRequest) -> Result<MediaReadResponse, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let path = self.recorded_path(&id.to_string())?;
        let kinds = if req.image_only {
            &[Modality::Image][..]
        } else {
            &[Modality::Image, Modality::Audio][..]
        };
        let (mime, bytes) =
            crate::engine::images::produced_media(&path, None, kinds).ok_or_else(|| {
                let noun = if req.image_only { "image" } else { "media" };
                RpcError::new(
                    -32602,
                    format!("the model has produced no {noun} in this session"),
                )
            })?;
        response(mime, &bytes, req.metadata_only)
    }
}

fn response(
    mime: String,
    bytes: &[u8],
    metadata_only: bool,
) -> Result<MediaReadResponse, RpcError> {
    let data_bytes = if metadata_only { 0 } else { bytes.len() };
    if !fits_response(data_bytes, mime.len()) {
        return Err(RpcError::new(
            -32602,
            "the produced media is too large to read over this connection",
        ));
    }
    Ok(MediaReadResponse {
        mime,
        sha256: kage_session::BlobStore::digest(bytes),
        data: if metadata_only {
            String::new()
        } else {
            base64::engine::general_purpose::STANDARD.encode(bytes)
        },
    })
}

/// Allows for base64 expansion and the MIME string's JSON escaping.
fn fits_response(bytes: usize, mime_bytes: usize) -> bool {
    base64::encoded_len(bytes, true)
        .and_then(|encoded| encoded.checked_add(mime_bytes.saturating_mul(6)))
        .is_some_and(|size| size <= MEDIA_FRAME_BYTES)
}

#[cfg(test)]
mod tests {
    use super::{MEDIA_FRAME_BYTES, fits_response, response};

    #[test]
    fn media_response_limit_accounts_for_base64_and_envelope() {
        let mime_bytes = "image/png".len();
        let max_bytes = (MEDIA_FRAME_BYTES - mime_bytes * 6) / 4 * 3;
        assert!(fits_response(max_bytes, mime_bytes));
        assert!(!fits_response(max_bytes + 1, mime_bytes));
        assert!(!fits_response(usize::MAX, mime_bytes));
        assert!(!fits_response(0, usize::MAX));
    }
    #[test]
    fn metadata_lookup_accepts_media_too_large_for_transfer() {
        let bytes = vec![42; 7 * 1024 * 1024];
        let mime = "audio/mpeg";
        let metadata = response(mime.into(), &bytes, true).unwrap();
        assert_eq!(metadata.mime, mime);
        assert_eq!(metadata.sha256, kage_session::BlobStore::digest(&bytes));
        assert!(metadata.data.is_empty());
        let error = response(mime.into(), &bytes, false).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("too large"));
    }
}
