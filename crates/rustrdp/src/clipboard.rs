//! Portable clipboard representations and bounded file content requests.

/// Alternative representations of the same clipboard selection.
///
/// HTML is a UTF-8 fragment, RTF is its original encoded byte stream, PNG is a
/// complete PNG image, and DIB is a Windows bitmap without the BMP file header.
/// Image decoding and operating-system clipboard ownership belong to the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    /// Unicode plain text.
    pub text: Option<String>,
    /// HTML fragment, without the Windows CF_HTML offset header.
    pub html: Option<String>,
    /// Rich Text Format document.
    pub rtf: Option<Vec<u8>>,
    /// PNG representation of an image.
    pub png: Option<Vec<u8>>,
    /// CF_DIB or CF_DIBV5 representation of an image.
    pub dib: Option<Vec<u8>>,
}

impl ClipboardContent {
    /// Whether no supported representation is present.
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.html.is_none()
            && self.rtf.is_none()
            && self.png.is_none()
            && self.dib.is_none()
    }

    /// Total size of the retained representations, in bytes.
    pub fn len(&self) -> usize {
        self.text
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(self.html.as_ref().map_or(0, String::len))
            .saturating_add(self.rtf.as_ref().map_or(0, Vec::len))
            .saturating_add(self.png.as_ref().map_or(0, Vec::len))
            .saturating_add(self.dib.as_ref().map_or(0, Vec::len))
    }
}

/// Maximum number of files and directories in one clipboard offer.
pub const MAX_CLIPBOARD_FILES: usize = 4096;

/// Maximum bytes carried by one clipboard file response.
/// The negotiated channel message limit can reduce this value further.
pub const MAX_CLIPBOARD_FILE_CHUNK: u32 = 1024 * 1024;

/// Metadata for one regular file or directory in a clipboard offer.
///
/// Keep local sources open until `ClipboardFilesReleased` is emitted. Remote
/// offers remain readable until `ClipboardRemoteFilesCancelled` is emitted.
/// Names are relative Windows paths. Absolute local paths remain with the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFile {
    /// Relative Unicode path using backslash separators, at most 259 UTF-16 units.
    pub name: String,
    /// Size of the retained source in bytes; zero for a directory.
    pub size: u64,
    /// A directory entry, including empty directories. No content reads are valid.
    pub directory: bool,
}

/// A bounded read of a previously offered local or remote file snapshot.
///
/// For a `ClipboardFileRequest` event, read exactly `length` bytes from `offset`
/// and return them using `Session::clipboard_file_contents`, or supply `None`
/// if the read fails or is cancelled. A `ClipboardFileContents` event carries
/// the corresponding remote read and its received data. The caller owns I/O
/// and deadlines; it must eventually complete or cancel every transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardFileRequest {
    /// Opaque session-local token; never substitute a remote stream identifier.
    pub id: u64,
    /// Local caller-supplied ID, or the opaque ID from a remote file offer.
    pub offer_id: u64,
    /// Zero-based index into that offer's file list.
    pub index: u32,
    /// Absolute byte offset within the retained file.
    pub offset: u64,
    /// Required response length, bounded by `MAX_CLIPBOARD_FILE_CHUNK`.
    pub length: u32,
}
