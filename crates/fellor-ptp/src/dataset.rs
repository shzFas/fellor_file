//! PTP / MTP datasets carried in data phases.

use crate::codes::{datatype as dt, format, prop};
use crate::error::{PtpError, PtpResult};
use crate::wire::{parse_datetime, Reader, Writer};
use std::time::SystemTime;

// ---------------------------------------------------------------- DeviceInfo

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub standard_version: u16,
    pub vendor_extension_id: u32,
    pub vendor_extension_version: u16,
    pub vendor_extension_desc: String,
    pub functional_mode: u16,
    pub operations_supported: Vec<u16>,
    pub events_supported: Vec<u16>,
    pub device_properties_supported: Vec<u16>,
    pub capture_formats: Vec<u16>,
    pub playback_formats: Vec<u16>,
    pub manufacturer: String,
    pub model: String,
    pub device_version: String,
    pub serial_number: String,
}

impl DeviceInfo {
    pub fn decode(buf: &[u8]) -> PtpResult<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            standard_version: r.u16()?,
            vendor_extension_id: r.u32()?,
            vendor_extension_version: r.u16()?,
            vendor_extension_desc: r.string()?,
            functional_mode: r.u16()?,
            operations_supported: r.u16_array()?,
            events_supported: r.u16_array()?,
            device_properties_supported: r.u16_array()?,
            capture_formats: r.u16_array()?,
            playback_formats: r.u16_array()?,
            manufacturer: r.string()?,
            model: r.string()?,
            device_version: r.string()?,
            // Some cameras end the dataset early.
            serial_number: if r.is_empty() { String::new() } else { r.string()? },
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16(self.standard_version)
            .u32(self.vendor_extension_id)
            .u16(self.vendor_extension_version)
            .string(&self.vendor_extension_desc)
            .u16(self.functional_mode)
            .u16_array(&self.operations_supported)
            .u16_array(&self.events_supported)
            .u16_array(&self.device_properties_supported)
            .u16_array(&self.capture_formats)
            .u16_array(&self.playback_formats)
            .string(&self.manufacturer)
            .string(&self.model)
            .string(&self.device_version)
            .string(&self.serial_number);
        w.into_vec()
    }

    pub fn supports_op(&self, code: u16) -> bool {
        self.operations_supported.contains(&code)
    }

    /// MTP device (Microsoft vendor extension id 6 or "microsoft.com" in
    /// the extension descriptor). Plain PTP cameras report neither.
    pub fn is_mtp(&self) -> bool {
        self.vendor_extension_id == 0x0000_0006
            || self.vendor_extension_desc.to_ascii_lowercase().contains("microsoft.com")
    }

    /// Device advertises the Android MTP extension (`android.com: 1.0`).
    pub fn has_android_extension(&self) -> bool {
        self.vendor_extension_desc.to_ascii_lowercase().contains("android.com")
    }
}

// --------------------------------------------------------------- StorageInfo

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageInfo {
    pub storage_type: u16,
    pub filesystem_type: u16,
    /// 0 = read-write, 1 = read-only without delete, 2 = read-only with delete.
    pub access_capability: u16,
    pub max_capacity: u64,
    pub free_space_bytes: u64,
    pub free_space_objects: u32,
    pub description: String,
    pub volume_label: String,
}

impl StorageInfo {
    pub fn decode(buf: &[u8]) -> PtpResult<Self> {
        let mut r = Reader::new(buf);
        Ok(Self {
            storage_type: r.u16()?,
            filesystem_type: r.u16()?,
            access_capability: r.u16()?,
            max_capacity: r.u64()?,
            free_space_bytes: r.u64()?,
            free_space_objects: r.u32()?,
            description: r.string()?,
            volume_label: if r.is_empty() { String::new() } else { r.string()? },
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16(self.storage_type)
            .u16(self.filesystem_type)
            .u16(self.access_capability)
            .u64(self.max_capacity)
            .u64(self.free_space_bytes)
            .u32(self.free_space_objects)
            .string(&self.description)
            .string(&self.volume_label);
        w.into_vec()
    }

    pub fn read_only(&self) -> bool {
        self.access_capability != 0
    }

    /// Human name: StorageDescription, else VolumeLabel.
    pub fn display_name(&self) -> Option<&str> {
        [self.description.trim(), self.volume_label.trim()].into_iter().find(|s| !s.is_empty())
    }
}

// ---------------------------------------------------------------- ObjectInfo

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectInfo {
    pub storage_id: u32,
    pub format: u16,
    pub protection_status: u16,
    /// 32-bit size; 0xFFFFFFFF for objects >= 4 GiB (use ObjectSize 0xDC04).
    pub compressed_size: u32,
    pub thumb_format: u16,
    pub thumb_compressed_size: u32,
    pub thumb_pix_width: u32,
    pub thumb_pix_height: u32,
    pub image_pix_width: u32,
    pub image_pix_height: u32,
    pub image_bit_depth: u32,
    pub parent: u32,
    pub association_type: u16,
    pub association_desc: u32,
    pub sequence_number: u32,
    pub filename: String,
    pub capture_date: String,
    pub modification_date: String,
    pub keywords: String,
}

impl ObjectInfo {
    pub fn decode(buf: &[u8]) -> PtpResult<Self> {
        let mut r = Reader::new(buf);
        let mut o = Self {
            storage_id: r.u32()?,
            format: r.u16()?,
            protection_status: r.u16()?,
            compressed_size: r.u32()?,
            thumb_format: r.u16()?,
            thumb_compressed_size: r.u32()?,
            thumb_pix_width: r.u32()?,
            thumb_pix_height: r.u32()?,
            image_pix_width: r.u32()?,
            image_pix_height: r.u32()?,
            image_bit_depth: r.u32()?,
            parent: r.u32()?,
            association_type: r.u16()?,
            association_desc: r.u32()?,
            sequence_number: r.u32()?,
            filename: r.string()?,
            ..Default::default()
        };
        // Trailing strings are optional on some cameras.
        for f in [&mut o.capture_date, &mut o.modification_date, &mut o.keywords] {
            if r.is_empty() {
                break;
            }
            *f = r.string()?;
        }
        Ok(o)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(self.storage_id)
            .u16(self.format)
            .u16(self.protection_status)
            .u32(self.compressed_size)
            .u16(self.thumb_format)
            .u32(self.thumb_compressed_size)
            .u32(self.thumb_pix_width)
            .u32(self.thumb_pix_height)
            .u32(self.image_pix_width)
            .u32(self.image_pix_height)
            .u32(self.image_bit_depth)
            .u32(self.parent)
            .u16(self.association_type)
            .u32(self.association_desc)
            .u32(self.sequence_number)
            .string(&self.filename)
            .string(&self.capture_date)
            .string(&self.modification_date)
            .string(&self.keywords);
        w.into_vec()
    }

    pub fn is_dir(&self) -> bool {
        self.format == format::ASSOCIATION
    }

    pub fn mtime(&self) -> Option<SystemTime> {
        parse_datetime(&self.modification_date)
    }

    pub fn ctime(&self) -> Option<SystemTime> {
        parse_datetime(&self.capture_date)
    }
}

// ------------------------------------------------------------- Prop values

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropValue {
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    I128(i128),
    U128(u128),
    Str(String),
    /// Array of the given element datatype.
    Array(u16, Vec<PropValue>),
}

impl PropValue {
    pub fn datatype(&self) -> u16 {
        match self {
            PropValue::I8(_) => dt::INT8,
            PropValue::U8(_) => dt::UINT8,
            PropValue::I16(_) => dt::INT16,
            PropValue::U16(_) => dt::UINT16,
            PropValue::I32(_) => dt::INT32,
            PropValue::U32(_) => dt::UINT32,
            PropValue::I64(_) => dt::INT64,
            PropValue::U64(_) => dt::UINT64,
            PropValue::I128(_) => dt::INT128,
            PropValue::U128(_) => dt::UINT128,
            PropValue::Str(_) => dt::STR,
            PropValue::Array(t, _) => dt::ARRAY | t,
        }
    }

    pub fn decode(r: &mut Reader<'_>, datatype: u16) -> PtpResult<PropValue> {
        Ok(match datatype {
            dt::INT8 => PropValue::I8(r.u8()? as i8),
            dt::UINT8 => PropValue::U8(r.u8()?),
            dt::INT16 => PropValue::I16(r.u16()? as i16),
            dt::UINT16 => PropValue::U16(r.u16()?),
            dt::INT32 => PropValue::I32(r.u32()? as i32),
            dt::UINT32 => PropValue::U32(r.u32()?),
            dt::INT64 => PropValue::I64(r.u64()? as i64),
            dt::UINT64 => PropValue::U64(r.u64()?),
            dt::INT128 => PropValue::I128(r.u128()? as i128),
            dt::UINT128 => PropValue::U128(r.u128()?),
            dt::STR => PropValue::Str(r.string()?),
            t if t & dt::ARRAY != 0 && t != dt::STR => {
                let elem = t & !dt::ARRAY;
                let size = elem_size(elem).ok_or_else(|| PtpError::protocol(format!("bad array type 0x{t:04X}")))?;
                let n = r.array_len(size)?;
                let items = (0..n).map(|_| PropValue::decode(r, elem)).collect::<PtpResult<Vec<_>>>()?;
                PropValue::Array(elem, items)
            }
            t => return Err(PtpError::protocol(format!("unknown datatype 0x{t:04X}"))),
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        match self {
            PropValue::I8(v) => {
                w.u8(*v as u8);
            }
            PropValue::U8(v) => {
                w.u8(*v);
            }
            PropValue::I16(v) => {
                w.u16(*v as u16);
            }
            PropValue::U16(v) => {
                w.u16(*v);
            }
            PropValue::I32(v) => {
                w.u32(*v as u32);
            }
            PropValue::U32(v) => {
                w.u32(*v);
            }
            PropValue::I64(v) => {
                w.u64(*v as u64);
            }
            PropValue::U64(v) => {
                w.u64(*v);
            }
            PropValue::I128(v) => {
                w.u128(*v as u128);
            }
            PropValue::U128(v) => {
                w.u128(*v);
            }
            PropValue::Str(s) => {
                w.string(s);
            }
            PropValue::Array(_, items) => {
                w.u32(items.len() as u32);
                for i in items {
                    i.encode(w);
                }
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode(&mut w);
        w.into_vec()
    }

    /// Integer value widened to u64 (negative values are rejected).
    pub fn as_u64(&self) -> Option<u64> {
        Some(match *self {
            PropValue::U8(v) => v as u64,
            PropValue::U16(v) => v as u64,
            PropValue::U32(v) => v as u64,
            PropValue::U64(v) => v,
            PropValue::U128(v) => u64::try_from(v).ok()?,
            PropValue::I8(v) => u64::try_from(v).ok()?,
            PropValue::I16(v) => u64::try_from(v).ok()?,
            PropValue::I32(v) => u64::try_from(v).ok()?,
            PropValue::I64(v) => u64::try_from(v).ok()?,
            PropValue::I128(v) => u64::try_from(v).ok()?,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

fn elem_size(t: u16) -> Option<usize> {
    Some(match t {
        dt::INT8 | dt::UINT8 => 1,
        dt::INT16 | dt::UINT16 => 2,
        dt::INT32 | dt::UINT32 => 4,
        dt::INT64 | dt::UINT64 => 8,
        dt::INT128 | dt::UINT128 => 16,
        _ => return None,
    })
}

// ------------------------------------------------------------ ObjectPropList

/// One element of an MTP ObjectPropList dataset (GetObjectPropList 0x9805,
/// SendObjectPropList 0x9808).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropListElement {
    pub handle: u32,
    pub prop: u16,
    pub value: PropValue,
}

/// `u32 count, count × { u32 handle, u16 prop code, u16 datatype, value }`.
pub fn decode_prop_list(buf: &[u8]) -> PtpResult<Vec<PropListElement>> {
    let mut r = Reader::new(buf);
    // Each element is at least 4+2+2+1 bytes.
    let n = r.array_len(9)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let handle = r.u32()?;
        let prop = r.u16()?;
        let datatype = r.u16()?;
        let value = PropValue::decode(&mut r, datatype)?;
        out.push(PropListElement { handle, prop, value });
    }
    Ok(out)
}

pub fn encode_prop_list(elems: &[PropListElement]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(elems.len() as u32);
    for e in elems {
        w.u32(e.handle).u16(e.prop).u16(e.value.datatype());
        e.value.encode(&mut w);
    }
    w.into_vec()
}

/// Object metadata assembled from a prop list (one per handle).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectProps {
    pub handle: u32,
    pub storage_id: Option<u32>,
    pub parent: Option<u32>,
    pub format: Option<u16>,
    pub size: Option<u64>,
    pub filename: Option<String>,
    pub date_modified: Option<String>,
    pub date_created: Option<String>,
    pub hidden: bool,
}

impl ObjectProps {
    pub fn is_dir(&self) -> bool {
        self.format == Some(format::ASSOCIATION)
    }

    pub fn apply(&mut self, prop_code: u16, v: &PropValue) {
        match prop_code {
            prop::STORAGE_ID => self.storage_id = v.as_u64().map(|x| x as u32),
            prop::PARENT_OBJECT => self.parent = v.as_u64().map(|x| x as u32),
            prop::OBJECT_FORMAT => self.format = v.as_u64().map(|x| x as u16),
            prop::OBJECT_SIZE => self.size = v.as_u64(),
            prop::OBJECT_FILE_NAME => self.filename = v.as_str().map(str::to_owned),
            prop::DATE_MODIFIED => self.date_modified = v.as_str().map(str::to_owned),
            prop::DATE_CREATED => self.date_created = v.as_str().map(str::to_owned),
            prop::HIDDEN => self.hidden = v.as_u64().unwrap_or(0) != 0,
            _ => {}
        }
    }

    /// Group prop-list elements by handle, preserving first-seen order.
    pub fn group(elems: &[PropListElement]) -> Vec<ObjectProps> {
        let mut order: Vec<ObjectProps> = Vec::new();
        let mut idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
        for e in elems {
            let i = *idx.entry(e.handle).or_insert_with(|| {
                order.push(ObjectProps { handle: e.handle, ..Default::default() });
                order.len() - 1
            });
            order[i].apply(e.prop, &e.value);
        }
        order
    }
}

// ----------------------------------------------------------------- Events

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub code: u16,
    pub tid: u32,
    pub params: Vec<u32>,
}

impl Event {
    pub fn decode(buf: &[u8]) -> PtpResult<Event> {
        let c = crate::container::ParamContainer::decode(buf)?;
        if c.kind != crate::container::ContainerType::Event {
            return Err(PtpError::protocol(format!("expected event container, got {:?}", c.kind)));
        }
        Ok(Event { code: c.code, tid: c.tid, params: c.params })
    }

    pub fn param(&self, i: usize) -> u32 {
        self.params.get(i).copied().unwrap_or(0)
    }
}

// ----------------------------------------------------- GetDeviceStatus reply

/// Reply to class request 0x67: `u16 wLength, u16 code, u32 params...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus {
    pub code: u16,
    pub params: Vec<u32>,
}

impl DeviceStatus {
    pub fn decode(buf: &[u8]) -> PtpResult<DeviceStatus> {
        let mut r = Reader::new(buf);
        let len = r.u16()? as usize;
        let code = r.u16()?;
        let len = len.min(buf.len());
        let mut params = Vec::new();
        while r.position() + 4 <= len {
            params.push(r.u32()?);
        }
        Ok(DeviceStatus { code, params })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codes::op;

    /// GetDeviceInfo payload in the shape a Pixel (Android 14) returns,
    /// hand-assembled byte by byte.
    fn pixel_device_info_bytes() -> Vec<u8> {
        let mut b = vec![
            0x64, 0x00, // StandardVersion 100
            0x06, 0x00, 0x00, 0x00, // VendorExtensionID 6 (Microsoft)
            0x64, 0x00, // VendorExtensionVersion 100
        ];
        // "microsoft.com: 1.0; android.com: 1.0; " -> 39 chars + NUL = 40
        let desc = "microsoft.com: 1.0; android.com: 1.0; ";
        b.push((desc.len() + 1) as u8);
        for c in desc.encode_utf16() {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&[0x00, 0x00]); // FunctionalMode
        // OperationsSupported: 4 ops
        b.extend_from_slice(&[0x04, 0, 0, 0, 0x01, 0x10, 0x02, 0x10, 0x05, 0x98, 0xC1, 0x95]);
        // EventsSupported: 2
        b.extend_from_slice(&[0x02, 0, 0, 0, 0x02, 0x40, 0x03, 0x40]);
        // DevicePropertiesSupported: 1 (0xD402 friendly name)
        b.extend_from_slice(&[0x01, 0, 0, 0, 0x02, 0xD4]);
        // CaptureFormats: 0
        b.extend_from_slice(&[0, 0, 0, 0]);
        // PlaybackFormats: 2 (Undefined, Association)
        b.extend_from_slice(&[0x02, 0, 0, 0, 0x00, 0x30, 0x01, 0x30]);
        for s in ["Google", "Pixel 8", "1.0", "37211FDJH00ABC"] {
            b.push((s.len() + 1) as u8);
            for c in s.encode_utf16() {
                b.extend_from_slice(&c.to_le_bytes());
            }
            b.extend_from_slice(&[0, 0]);
        }
        b
    }

    #[test]
    fn device_info_decode() {
        let raw = pixel_device_info_bytes();
        let di = DeviceInfo::decode(&raw).unwrap();
        assert_eq!(di.standard_version, 100);
        assert_eq!(di.vendor_extension_id, 6);
        assert!(di.is_mtp());
        assert!(di.has_android_extension());
        assert_eq!(di.operations_supported, vec![0x1001, 0x1002, 0x9805, 0x95C1]);
        assert!(di.supports_op(op::GET_OBJECT_PROP_LIST));
        assert!(di.supports_op(op::ANDROID_GET_PARTIAL_OBJECT_64));
        assert_eq!(di.events_supported, vec![0x4002, 0x4003]);
        assert_eq!(di.playback_formats, vec![0x3000, 0x3001]);
        assert_eq!(di.manufacturer, "Google");
        assert_eq!(di.model, "Pixel 8");
        assert_eq!(di.device_version, "1.0");
        assert_eq!(di.serial_number, "37211FDJH00ABC");
        // Encoder produces identical bytes.
        assert_eq!(di.encode(), raw);
    }

    #[test]
    fn camera_device_info_is_not_mtp() {
        let di = DeviceInfo {
            standard_version: 100,
            vendor_extension_id: 0x0B, // Canon
            vendor_extension_desc: String::new(),
            manufacturer: "Canon Inc.".into(),
            model: "Canon EOS R6".into(),
            ..Default::default()
        };
        let back = DeviceInfo::decode(&di.encode()).unwrap();
        assert_eq!(back, di);
        assert!(!back.is_mtp());
        assert!(!back.has_android_extension());
    }

    #[test]
    fn storage_info_bytes() {
        let mut raw = vec![
            0x03, 0x00, // FixedRAM
            0x02, 0x00, // Generic hierarchical
            0x00, 0x00, // read-write
        ];
        raw.extend_from_slice(&(128u64 * 1_000_000_000).to_le_bytes());
        raw.extend_from_slice(&(42u64 * 1_000_000_000).to_le_bytes());
        raw.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let name = "Internal shared storage";
        raw.push((name.len() + 1) as u8);
        for c in name.encode_utf16() {
            raw.extend_from_slice(&c.to_le_bytes());
        }
        raw.extend_from_slice(&[0, 0]);
        raw.push(0); // empty volume label
        let si = StorageInfo::decode(&raw).unwrap();
        assert_eq!(si.storage_type, 3);
        assert_eq!(si.max_capacity, 128_000_000_000);
        assert_eq!(si.free_space_bytes, 42_000_000_000);
        assert_eq!(si.display_name(), Some("Internal shared storage"));
        assert!(!si.read_only());
        assert_eq!(si.encode(), raw);
    }

    #[test]
    fn storage_name_falls_back_to_label() {
        let si = StorageInfo { volume_label: "SD card".into(), ..Default::default() };
        assert_eq!(si.display_name(), Some("SD card"));
        assert_eq!(StorageInfo::default().display_name(), None);
    }

    #[test]
    fn object_info_roundtrip_and_bytes() {
        let oi = ObjectInfo {
            storage_id: 0x0001_0001,
            format: format::EXIF_JPEG,
            compressed_size: 0x0012_3456,
            parent: 0x0000_0010,
            filename: "IMG_1.jpg".into(),
            modification_date: "20240131T235959Z".into(),
            ..Default::default()
        };
        let raw = oi.encode();
        // fixed part: 4+2+2+4+2+4*6+4+2+4+4 = 52 bytes
        assert_eq!(&raw[0..4], &[0x01, 0x00, 0x01, 0x00]);
        assert_eq!(&raw[4..6], &[0x01, 0x38]);
        assert_eq!(&raw[8..12], &[0x56, 0x34, 0x12, 0x00]);
        assert_eq!(&raw[38..42], &[0x10, 0, 0, 0]); // ParentObject
        assert_eq!(raw[52], 10); // filename count incl NUL
        let back = ObjectInfo::decode(&raw).unwrap();
        assert_eq!(back, oi);
        assert!(!back.is_dir());
        assert!(back.mtime().is_some());
    }

    #[test]
    fn object_info_without_trailing_strings() {
        let oi = ObjectInfo { filename: "DCIM".into(), format: format::ASSOCIATION, ..Default::default() };
        let mut raw = oi.encode();
        raw.truncate(52 + 1 + 5 * 2); // drop dates/keywords
        let back = ObjectInfo::decode(&raw).unwrap();
        assert_eq!(back.filename, "DCIM");
        assert!(back.is_dir());
        assert_eq!(back.modification_date, "");
    }

    #[test]
    fn prop_list_bytes() {
        // Two elements: handle 7 ObjectFileName "a.txt", handle 7 ObjectSize u64 5 GiB
        let mut raw = vec![0x02, 0, 0, 0];
        raw.extend_from_slice(&[0x07, 0, 0, 0, 0x07, 0xDC, 0xFF, 0xFF]);
        raw.push(6);
        for c in "a.txt".encode_utf16() {
            raw.extend_from_slice(&c.to_le_bytes());
        }
        raw.extend_from_slice(&[0, 0]);
        raw.extend_from_slice(&[0x07, 0, 0, 0, 0x04, 0xDC, 0x08, 0x00]);
        raw.extend_from_slice(&(5u64 << 30).to_le_bytes());
        let elems = decode_prop_list(&raw).unwrap();
        assert_eq!(elems.len(), 2);
        assert_eq!(elems[0], PropListElement { handle: 7, prop: prop::OBJECT_FILE_NAME, value: PropValue::Str("a.txt".into()) });
        assert_eq!(elems[1].value, PropValue::U64(5 << 30));
        assert_eq!(encode_prop_list(&elems), raw);
        let objs = ObjectProps::group(&elems);
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].filename.as_deref(), Some("a.txt"));
        assert_eq!(objs[0].size, Some(5 << 30));
    }

    #[test]
    fn prop_list_all_types_roundtrip() {
        let vals = vec![
            PropValue::I8(-3),
            PropValue::U8(200),
            PropValue::I16(-300),
            PropValue::U16(0xDC07),
            PropValue::I32(-70000),
            PropValue::U32(0xDEADBEEF),
            PropValue::I64(-1),
            PropValue::U64(u64::MAX),
            PropValue::I128(-5),
            PropValue::U128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10),
            PropValue::Str("ё".into()),
            PropValue::Array(dt::UINT16, vec![PropValue::U16(1), PropValue::U16(2)]),
        ];
        let elems: Vec<_> =
            vals.into_iter().enumerate().map(|(i, value)| PropListElement { handle: i as u32, prop: 0xDC00 + i as u16, value }).collect();
        let raw = encode_prop_list(&elems);
        assert_eq!(decode_prop_list(&raw).unwrap(), elems);
    }

    #[test]
    fn prop_list_grouping_and_unknown_datatype() {
        let elems = vec![
            PropListElement { handle: 1, prop: prop::OBJECT_FILE_NAME, value: PropValue::Str("x".into()) },
            PropListElement { handle: 2, prop: prop::OBJECT_FORMAT, value: PropValue::U16(format::ASSOCIATION) },
            PropListElement { handle: 1, prop: prop::PARENT_OBJECT, value: PropValue::U32(0) },
            PropListElement { handle: 1, prop: prop::STORAGE_ID, value: PropValue::U32(0x10001) },
            PropListElement { handle: 2, prop: prop::HIDDEN, value: PropValue::U16(1) },
        ];
        let g = ObjectProps::group(&elems);
        assert_eq!(g.iter().map(|o| o.handle).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(g[0].parent, Some(0));
        assert_eq!(g[0].storage_id, Some(0x10001));
        assert!(g[1].is_dir());
        assert!(g[1].hidden);
        let bad = [0x01, 0, 0, 0, 1, 0, 0, 0, 0x07, 0xDC, 0x34, 0x12, 0];
        assert!(decode_prop_list(&bad).is_err());
    }

    #[test]
    fn device_status_decode() {
        let raw = [0x04, 0x00, 0x01, 0x20];
        assert_eq!(DeviceStatus::decode(&raw).unwrap(), DeviceStatus { code: 0x2001, params: vec![] });
        let raw = [0x0C, 0x00, 0x19, 0x20, 0x81, 0, 0, 0, 0x02, 0, 0, 0];
        let s = DeviceStatus::decode(&raw).unwrap();
        assert_eq!(s.code, 0x2019);
        assert_eq!(s.params, vec![0x81, 0x02]);
    }
}
