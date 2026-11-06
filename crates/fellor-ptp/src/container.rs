//! PTP-over-USB generic container:
//! `length:u32 | type:u16 | code:u16 | transaction_id:u32 | payload`.

use crate::error::{PtpError, PtpResult};

pub const HEADER_LEN: usize = 12;
/// Length value used by devices/hosts when the container is >= 4 GiB.
pub const LEN_UNKNOWN: u32 = 0xFFFF_FFFF;
pub const MAX_PARAMS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ContainerType {
    Command = 1,
    Data = 2,
    Response = 3,
    Event = 4,
}

impl ContainerType {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Command,
            2 => Self::Data,
            3 => Self::Response,
            4 => Self::Event,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Raw length field (total container bytes, or [`LEN_UNKNOWN`]).
    pub length: u32,
    pub kind: ContainerType,
    pub code: u16,
    pub tid: u32,
}

impl Header {
    pub fn decode(buf: &[u8]) -> PtpResult<Header> {
        if buf.len() < HEADER_LEN {
            return Err(PtpError::protocol(format!("container too short ({} bytes)", buf.len())));
        }
        let length = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let kind_raw = u16::from_le_bytes(buf[4..6].try_into().unwrap());
        let kind = ContainerType::from_u16(kind_raw)
            .ok_or_else(|| PtpError::protocol(format!("bad container type {kind_raw}")))?;
        let code = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        let tid = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if length != LEN_UNKNOWN && (length as usize) < HEADER_LEN {
            return Err(PtpError::protocol(format!("container length {length} < header")));
        }
        Ok(Header { length, kind, code, tid })
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&self.length.to_le_bytes());
        b[4..6].copy_from_slice(&(self.kind as u16).to_le_bytes());
        b[6..8].copy_from_slice(&self.code.to_le_bytes());
        b[8..12].copy_from_slice(&self.tid.to_le_bytes());
        b
    }

    /// Payload length if the header states it.
    pub fn payload_len(&self) -> Option<u64> {
        (self.length != LEN_UNKNOWN).then(|| self.length as u64 - HEADER_LEN as u64)
    }

    /// Data container header for a payload of `payload_len` bytes; uses
    /// [`LEN_UNKNOWN`] when the total does not fit in u32.
    pub fn data(code: u16, tid: u32, payload_len: u64) -> Header {
        let total = payload_len + HEADER_LEN as u64;
        let length = if total >= LEN_UNKNOWN as u64 { LEN_UNKNOWN } else { total as u32 };
        Header { length, kind: ContainerType::Data, code, tid }
    }
}

/// Encode a command/response/event container with u32 parameters.
pub fn encode_params(kind: ContainerType, code: u16, tid: u32, params: &[u32]) -> Vec<u8> {
    assert!(params.len() <= MAX_PARAMS, "at most 5 parameters");
    let len = HEADER_LEN + params.len() * 4;
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&Header { length: len as u32, kind, code, tid }.encode());
    for p in params {
        v.extend_from_slice(&p.to_le_bytes());
    }
    v
}

pub fn encode_command(code: u16, tid: u32, params: &[u32]) -> Vec<u8> {
    encode_params(ContainerType::Command, code, tid, params)
}

/// A decoded command, response or event (all share the params layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamContainer {
    pub kind: ContainerType,
    pub code: u16,
    pub tid: u32,
    pub params: Vec<u32>,
}

impl ParamContainer {
    pub fn decode(buf: &[u8]) -> PtpResult<ParamContainer> {
        let h = Header::decode(buf)?;
        if h.kind == ContainerType::Data {
            return Err(PtpError::protocol("expected a parameter container, got data"));
        }
        let len = if h.length == LEN_UNKNOWN { buf.len() } else { h.length as usize };
        if buf.len() < len {
            return Err(PtpError::protocol(format!("container truncated: {} < {len}", buf.len())));
        }
        let body = &buf[HEADER_LEN..len];
        // Tolerate trailing garbage that is not a whole u32.
        let params: Vec<u32> = body
            .chunks_exact(4)
            .take(MAX_PARAMS)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        Ok(ParamContainer { kind: h.kind, code: h.code, tid: h.tid, params })
    }

    pub fn param(&self, i: usize) -> u32 {
        self.params.get(i).copied().unwrap_or(0)
    }
}

/// Device response to a transaction.
pub type Response = ParamContainer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_bytes() {
        // OpenSession(session_id = 1), tid 0
        let v = encode_command(0x1002, 0, &[1]);
        assert_eq!(v, [0x10, 0, 0, 0, 0x01, 0x00, 0x02, 0x10, 0, 0, 0, 0, 0x01, 0, 0, 0]);
        // GetObjectHandles(storage=0x00010001, format=0, parent=0xFFFFFFFF), tid 7
        let v = encode_command(0x1007, 7, &[0x0001_0001, 0, 0xFFFF_FFFF]);
        assert_eq!(
            v,
            [
                0x18, 0, 0, 0, 0x01, 0, 0x07, 0x10, 0x07, 0, 0, 0, 0x01, 0, 0x01, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF,
                0xFF
            ]
        );
    }

    #[test]
    fn response_decode() {
        // OK, tid 5, one param (new handle 0x2A)
        let raw = [0x10, 0, 0, 0, 0x03, 0, 0x01, 0x20, 0x05, 0, 0, 0, 0x2A, 0, 0, 0];
        let r = ParamContainer::decode(&raw).unwrap();
        assert_eq!(r.kind, ContainerType::Response);
        assert_eq!(r.code, 0x2001);
        assert_eq!(r.tid, 5);
        assert_eq!(r.params, vec![0x2A]);
        assert_eq!(r.param(3), 0);
    }

    #[test]
    fn response_with_five_params() {
        let v = encode_params(ContainerType::Response, 0x2001, 9, &[1, 2, 3, 4, 5]);
        assert_eq!(v.len(), 32);
        assert_eq!(ParamContainer::decode(&v).unwrap().params, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn event_decode() {
        // ObjectAdded(handle 0x123) on the interrupt pipe
        let raw = [0x10, 0, 0, 0, 0x04, 0, 0x02, 0x40, 0xFF, 0xFF, 0xFF, 0xFF, 0x23, 0x01, 0, 0];
        let e = ParamContainer::decode(&raw).unwrap();
        assert_eq!(e.kind, ContainerType::Event);
        assert_eq!(e.code, 0x4002);
        assert_eq!(e.params, vec![0x123]);
    }

    #[test]
    fn data_header_lengths() {
        let h = Header::data(0x1009, 3, 100);
        assert_eq!(h.length, 112);
        assert_eq!(h.payload_len(), Some(100));
        assert_eq!(h.encode(), [112, 0, 0, 0, 2, 0, 0x09, 0x10, 3, 0, 0, 0]);
        // >4 GiB object: length field is 0xFFFFFFFF
        let big = Header::data(0x100D, 4, 5 * 1024 * 1024 * 1024);
        assert_eq!(big.length, LEN_UNKNOWN);
        assert_eq!(big.payload_len(), None);
        assert_eq!(&big.encode()[0..4], &[0xFF; 4]);
        // Boundary: total exactly u32::MAX - 1 still fits
        let edge = Header::data(0x100D, 4, 0xFFFF_FFFE - 12);
        assert_eq!(edge.length, 0xFFFF_FFFE);
        let edge2 = Header::data(0x100D, 4, 0xFFFF_FFFF - 12);
        assert_eq!(edge2.length, LEN_UNKNOWN);
    }

    #[test]
    fn decode_ffff_header() {
        let raw = [0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0x00, 0x09, 0x10, 0x11, 0, 0, 0];
        let h = Header::decode(&raw).unwrap();
        assert_eq!(h.length, LEN_UNKNOWN);
        assert_eq!(h.kind, ContainerType::Data);
        assert_eq!(h.code, 0x1009);
        assert_eq!(h.tid, 0x11);
    }

    #[test]
    fn bad_containers_rejected() {
        assert!(Header::decode(&[1, 2, 3]).is_err());
        assert!(Header::decode(&[12, 0, 0, 0, 9, 0, 1, 0x20, 0, 0, 0, 0]).is_err()); // type 9
        assert!(Header::decode(&[4, 0, 0, 0, 3, 0, 1, 0x20, 0, 0, 0, 0]).is_err()); // length < 12
        assert!(ParamContainer::decode(&[0x14, 0, 0, 0, 3, 0, 1, 0x20, 0, 0, 0, 0]).is_err()); // truncated
    }
}
