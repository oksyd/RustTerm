//! Device I/O envelopes. File-system access and permissions belong to the caller.

/// An MS-RDPEFS request for one explicitly announced device. Payload is bounded
/// by the configured virtual-channel limit and includes only the IRP body.
#[derive(Debug, PartialEq, Eq)]
pub struct DriveRequest {
    /// Device-negotiation epoch; return unchanged in the completion.
    pub generation: u64,
    /// One-based index into the explicitly configured shared drives.
    pub device_id: u32,
    /// Caller-issued handle returned by a successful create request.
    pub file_id: u32,
    /// Server-issued identity, unique while the request is pending.
    pub completion_id: u32,
    /// Windows IRP major function code.
    pub major: u32,
    /// Windows IRP minor function code.
    pub minor: u32,
    /// Bounded request or response body without the device I/O envelope.
    pub data: Vec<u8>,
}

/// An MS-RDPEFS completion. `status` is an NTSTATUS; `data` is the response body
/// prescribed for the original major/minor function, including error padding.
#[derive(Debug, PartialEq, Eq)]
pub struct DriveCompletion {
    /// Device-negotiation epoch; return unchanged in the completion.
    pub generation: u64,
    /// One-based index into the explicitly configured shared drives.
    pub device_id: u32,
    /// Server-issued identity, unique while the request is pending.
    pub completion_id: u32,
    /// Windows NTSTATUS describing the operation result.
    pub status: u32,
    /// Bounded request or response body without the device I/O envelope.
    pub data: Vec<u8>,
}
