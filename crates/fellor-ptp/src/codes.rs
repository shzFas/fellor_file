//! PTP (ISO 15740 / PIMA 15740:2000) and MTP 1.1 code tables.
//!
//! Values checked against the MTP 1.1 specification and the Android
//! `mtp.h` header (for the Android vendor extension `android.com`).

/// Operation codes.
pub mod op {
    pub const GET_DEVICE_INFO: u16 = 0x1001;
    pub const OPEN_SESSION: u16 = 0x1002;
    pub const CLOSE_SESSION: u16 = 0x1003;
    pub const GET_STORAGE_IDS: u16 = 0x1004;
    pub const GET_STORAGE_INFO: u16 = 0x1005;
    pub const GET_NUM_OBJECTS: u16 = 0x1006;
    pub const GET_OBJECT_HANDLES: u16 = 0x1007;
    pub const GET_OBJECT_INFO: u16 = 0x1008;
    pub const GET_OBJECT: u16 = 0x1009;
    pub const GET_THUMB: u16 = 0x100A;
    pub const DELETE_OBJECT: u16 = 0x100B;
    pub const SEND_OBJECT_INFO: u16 = 0x100C;
    pub const SEND_OBJECT: u16 = 0x100D;
    pub const MOVE_OBJECT: u16 = 0x1019;
    pub const COPY_OBJECT: u16 = 0x101A;
    pub const GET_PARTIAL_OBJECT: u16 = 0x101B;
    // MTP
    pub const GET_OBJECT_PROPS_SUPPORTED: u16 = 0x9801;
    pub const GET_OBJECT_PROP_DESC: u16 = 0x9802;
    pub const GET_OBJECT_PROP_VALUE: u16 = 0x9803;
    pub const SET_OBJECT_PROP_VALUE: u16 = 0x9804;
    pub const GET_OBJECT_PROP_LIST: u16 = 0x9805;
    pub const SET_OBJECT_PROP_LIST: u16 = 0x9806;
    pub const SEND_OBJECT_PROP_LIST: u16 = 0x9808;
    // Android extension ("android.com: 1.0"). NOTE: GetPartialObject64 is
    // 0x95C1 — 0x95C4 (as written in SPEC.md) is BeginEditObject.
    pub const ANDROID_GET_PARTIAL_OBJECT_64: u16 = 0x95C1;
    pub const ANDROID_SEND_PARTIAL_OBJECT: u16 = 0x95C2;
    pub const ANDROID_TRUNCATE_OBJECT: u16 = 0x95C3;
    pub const ANDROID_BEGIN_EDIT_OBJECT: u16 = 0x95C4;
    pub const ANDROID_END_EDIT_OBJECT: u16 = 0x95C5;

    pub fn name(code: u16) -> &'static str {
        match code {
            GET_DEVICE_INFO => "GetDeviceInfo",
            OPEN_SESSION => "OpenSession",
            CLOSE_SESSION => "CloseSession",
            GET_STORAGE_IDS => "GetStorageIDs",
            GET_STORAGE_INFO => "GetStorageInfo",
            GET_NUM_OBJECTS => "GetNumObjects",
            GET_OBJECT_HANDLES => "GetObjectHandles",
            GET_OBJECT_INFO => "GetObjectInfo",
            GET_OBJECT => "GetObject",
            GET_THUMB => "GetThumb",
            DELETE_OBJECT => "DeleteObject",
            SEND_OBJECT_INFO => "SendObjectInfo",
            SEND_OBJECT => "SendObject",
            MOVE_OBJECT => "MoveObject",
            COPY_OBJECT => "CopyObject",
            GET_PARTIAL_OBJECT => "GetPartialObject",
            GET_OBJECT_PROPS_SUPPORTED => "GetObjectPropsSupported",
            GET_OBJECT_PROP_DESC => "GetObjectPropDesc",
            GET_OBJECT_PROP_VALUE => "GetObjectPropValue",
            SET_OBJECT_PROP_VALUE => "SetObjectPropValue",
            GET_OBJECT_PROP_LIST => "GetObjectPropList",
            SET_OBJECT_PROP_LIST => "SetObjectPropList",
            SEND_OBJECT_PROP_LIST => "SendObjectPropList",
            ANDROID_GET_PARTIAL_OBJECT_64 => "GetPartialObject64",
            ANDROID_SEND_PARTIAL_OBJECT => "SendPartialObject",
            ANDROID_TRUNCATE_OBJECT => "TruncateObject",
            ANDROID_BEGIN_EDIT_OBJECT => "BeginEditObject",
            ANDROID_END_EDIT_OBJECT => "EndEditObject",
            _ => "Unknown",
        }
    }
}

/// Response codes.
pub mod resp {
    pub const OK: u16 = 0x2001;
    pub const GENERAL_ERROR: u16 = 0x2002;
    pub const SESSION_NOT_OPEN: u16 = 0x2003;
    pub const INVALID_TRANSACTION_ID: u16 = 0x2004;
    pub const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
    pub const PARAMETER_NOT_SUPPORTED: u16 = 0x2006;
    pub const INCOMPLETE_TRANSFER: u16 = 0x2007;
    pub const INVALID_STORAGE_ID: u16 = 0x2008;
    pub const INVALID_OBJECT_HANDLE: u16 = 0x2009;
    pub const DEVICE_PROP_NOT_SUPPORTED: u16 = 0x200A;
    pub const INVALID_OBJECT_FORMAT_CODE: u16 = 0x200B;
    pub const STORE_FULL: u16 = 0x200C;
    pub const OBJECT_WRITE_PROTECTED: u16 = 0x200D;
    pub const STORE_READ_ONLY: u16 = 0x200E;
    pub const ACCESS_DENIED: u16 = 0x200F;
    pub const NO_THUMBNAIL_PRESENT: u16 = 0x2010;
    pub const PARTIAL_DELETION: u16 = 0x2012;
    pub const STORE_NOT_AVAILABLE: u16 = 0x2013;
    pub const SPECIFICATION_BY_FORMAT_UNSUPPORTED: u16 = 0x2014;
    pub const NO_VALID_OBJECT_INFO: u16 = 0x2015;
    pub const DEVICE_BUSY: u16 = 0x2019;
    pub const INVALID_PARENT_OBJECT: u16 = 0x201A;
    pub const INVALID_PARAMETER: u16 = 0x201D;
    pub const SESSION_ALREADY_OPEN: u16 = 0x201E;
    pub const TRANSACTION_CANCELLED: u16 = 0x201F;
    pub const SPECIFICATION_OF_DESTINATION_UNSUPPORTED: u16 = 0x2020;
    // MTP
    pub const INVALID_OBJECT_PROP_CODE: u16 = 0xA801;
    pub const INVALID_OBJECT_PROP_FORMAT: u16 = 0xA802;
    pub const INVALID_OBJECT_PROP_VALUE: u16 = 0xA803;
    pub const INVALID_OBJECT_REFERENCE: u16 = 0xA804;
    pub const GROUP_NOT_SUPPORTED: u16 = 0xA805;
    pub const INVALID_DATASET: u16 = 0xA806;
    pub const SPECIFICATION_BY_GROUP_UNSUPPORTED: u16 = 0xA807;
    pub const SPECIFICATION_BY_DEPTH_UNSUPPORTED: u16 = 0xA808;
    pub const OBJECT_TOO_LARGE: u16 = 0xA809;
    pub const OBJECT_PROP_NOT_SUPPORTED: u16 = 0xA80A;

    pub fn name(code: u16) -> &'static str {
        match code {
            OK => "OK",
            GENERAL_ERROR => "General_Error",
            SESSION_NOT_OPEN => "Session_Not_Open",
            INVALID_TRANSACTION_ID => "Invalid_TransactionID",
            OPERATION_NOT_SUPPORTED => "Operation_Not_Supported",
            PARAMETER_NOT_SUPPORTED => "Parameter_Not_Supported",
            INCOMPLETE_TRANSFER => "Incomplete_Transfer",
            INVALID_STORAGE_ID => "Invalid_StorageID",
            INVALID_OBJECT_HANDLE => "Invalid_ObjectHandle",
            DEVICE_PROP_NOT_SUPPORTED => "DeviceProp_Not_Supported",
            INVALID_OBJECT_FORMAT_CODE => "Invalid_ObjectFormatCode",
            STORE_FULL => "Store_Full",
            OBJECT_WRITE_PROTECTED => "Object_WriteProtected",
            STORE_READ_ONLY => "Store_Read_Only",
            ACCESS_DENIED => "Access_Denied",
            NO_THUMBNAIL_PRESENT => "No_Thumbnail_Present",
            PARTIAL_DELETION => "Partial_Deletion",
            STORE_NOT_AVAILABLE => "Store_Not_Available",
            SPECIFICATION_BY_FORMAT_UNSUPPORTED => "Specification_By_Format_Unsupported",
            NO_VALID_OBJECT_INFO => "No_Valid_ObjectInfo",
            DEVICE_BUSY => "Device_Busy",
            INVALID_PARENT_OBJECT => "Invalid_ParentObject",
            INVALID_PARAMETER => "Invalid_Parameter",
            SESSION_ALREADY_OPEN => "Session_Already_Open",
            TRANSACTION_CANCELLED => "Transaction_Cancelled",
            SPECIFICATION_OF_DESTINATION_UNSUPPORTED => "Specification_of_Destination_Unsupported",
            INVALID_OBJECT_PROP_CODE => "Invalid_ObjectPropCode",
            INVALID_OBJECT_PROP_FORMAT => "Invalid_ObjectProp_Format",
            INVALID_OBJECT_PROP_VALUE => "Invalid_ObjectProp_Value",
            INVALID_OBJECT_REFERENCE => "Invalid_ObjectReference",
            GROUP_NOT_SUPPORTED => "Group_Not_Supported",
            INVALID_DATASET => "Invalid_Dataset",
            SPECIFICATION_BY_GROUP_UNSUPPORTED => "Specification_By_Group_Unsupported",
            SPECIFICATION_BY_DEPTH_UNSUPPORTED => "Specification_By_Depth_Unsupported",
            OBJECT_TOO_LARGE => "Object_Too_Large",
            OBJECT_PROP_NOT_SUPPORTED => "ObjectProp_Not_Supported",
            _ => "Unknown",
        }
    }
}

/// Event codes.
pub mod event {
    pub const CANCEL_TRANSACTION: u16 = 0x4001;
    pub const OBJECT_ADDED: u16 = 0x4002;
    pub const OBJECT_REMOVED: u16 = 0x4003;
    pub const STORE_ADDED: u16 = 0x4004;
    pub const STORE_REMOVED: u16 = 0x4005;
    pub const DEVICE_PROP_CHANGED: u16 = 0x4006;
    pub const OBJECT_INFO_CHANGED: u16 = 0x4007;
    pub const DEVICE_INFO_CHANGED: u16 = 0x4008;
    pub const STORE_FULL: u16 = 0x400A;
    pub const DEVICE_RESET: u16 = 0x400B;
    pub const STORAGE_INFO_CHANGED: u16 = 0x400C;
    pub const UNREPORTED_STATUS: u16 = 0x400E;
    // MTP
    pub const OBJECT_PROP_CHANGED: u16 = 0xC801;
    pub const OBJECT_PROP_DESC_CHANGED: u16 = 0xC802;
    pub const OBJECT_REFERENCES_CHANGED: u16 = 0xC803;
}

/// Object format codes.
pub mod format {
    pub const UNDEFINED: u16 = 0x3000;
    pub const ASSOCIATION: u16 = 0x3001;
    pub const SCRIPT: u16 = 0x3002;
    pub const EXECUTABLE: u16 = 0x3003;
    pub const TEXT: u16 = 0x3004;
    pub const HTML: u16 = 0x3005;
    pub const DPOF: u16 = 0x3006;
    pub const AIFF: u16 = 0x3007;
    pub const WAV: u16 = 0x3008;
    pub const MP3: u16 = 0x3009;
    pub const AVI: u16 = 0x300A;
    pub const MPEG: u16 = 0x300B;
    pub const ASF: u16 = 0x300C;
    pub const UNDEFINED_IMAGE: u16 = 0x3800;
    pub const EXIF_JPEG: u16 = 0x3801;
    pub const TIFF_EP: u16 = 0x3802;
    pub const BMP: u16 = 0x3804;
    pub const GIF: u16 = 0x3807;
    pub const JFIF: u16 = 0x3808;
    pub const PNG: u16 = 0x380B;
    pub const TIFF: u16 = 0x380D;
    pub const JP2: u16 = 0x380F;
    pub const DNG: u16 = 0x3811;
    pub const HEIF: u16 = 0x3812;
    pub const WMA: u16 = 0xB901;
    pub const OGG: u16 = 0xB902;
    pub const AAC: u16 = 0xB903;
    pub const FLAC: u16 = 0xB906;
    pub const WMV: u16 = 0xB981;
    pub const MP4_CONTAINER: u16 = 0xB982;
    pub const MP2: u16 = 0xB983;
    pub const THREE_GP: u16 = 0xB984;

    /// Best-effort MIME type for a format code (used when the name has no extension).
    pub fn mime(code: u16) -> Option<&'static str> {
        Some(match code {
            EXIF_JPEG | JFIF => "image/jpeg",
            PNG => "image/png",
            GIF => "image/gif",
            BMP => "image/bmp",
            TIFF | TIFF_EP => "image/tiff",
            DNG => "image/x-adobe-dng",
            HEIF => "image/heic",
            MP3 => "audio/mpeg",
            WAV => "audio/wav",
            AAC => "audio/aac",
            OGG => "audio/ogg",
            FLAC => "audio/flac",
            AVI => "video/x-msvideo",
            MPEG => "video/mpeg",
            MP4_CONTAINER => "video/mp4",
            THREE_GP => "video/3gpp",
            TEXT => "text/plain",
            HTML => "text/html",
            _ => return None,
        })
    }

    /// Format code to declare when uploading a file with this name.
    pub fn from_name(name: &str) -> u16 {
        let ext = match name.rsplit_once('.') {
            Some((_, e)) => e.to_ascii_lowercase(),
            None => return UNDEFINED,
        };
        match ext.as_str() {
            "jpg" | "jpeg" => EXIF_JPEG,
            "png" => PNG,
            "gif" => GIF,
            "bmp" => BMP,
            "tif" | "tiff" => TIFF,
            "dng" => DNG,
            "heic" | "heif" => HEIF,
            "mp3" => MP3,
            "wav" => WAV,
            "aac" | "m4a" => AAC,
            "ogg" | "oga" => OGG,
            "flac" => FLAC,
            "avi" => AVI,
            "mpg" | "mpeg" => MPEG,
            "mp4" | "m4v" => MP4_CONTAINER,
            "3gp" => THREE_GP,
            "txt" => TEXT,
            "htm" | "html" => HTML,
            _ => UNDEFINED,
        }
    }
}

/// MTP object property codes.
///
/// NOTE (SPEC correction): there is no "ObjectCompressedSize64" property.
/// MTP's ObjectSize is 0xDC04 and is a UINT64 — it is what SendObjectPropList
/// carries for files >= 4 GiB. ObjectInfo's 32-bit ObjectCompressedSize is
/// set to 0xFFFFFFFF in that case.
pub mod prop {
    pub const STORAGE_ID: u16 = 0xDC01;
    pub const OBJECT_FORMAT: u16 = 0xDC02;
    pub const PROTECTION_STATUS: u16 = 0xDC03;
    pub const OBJECT_SIZE: u16 = 0xDC04;
    pub const ASSOCIATION_TYPE: u16 = 0xDC05;
    pub const ASSOCIATION_DESC: u16 = 0xDC06;
    pub const OBJECT_FILE_NAME: u16 = 0xDC07;
    pub const DATE_CREATED: u16 = 0xDC08;
    pub const DATE_MODIFIED: u16 = 0xDC09;
    pub const KEYWORDS: u16 = 0xDC0A;
    pub const PARENT_OBJECT: u16 = 0xDC0B;
    pub const ALLOWED_FOLDER_CONTENTS: u16 = 0xDC0C;
    pub const HIDDEN: u16 = 0xDC0D;
    pub const SYSTEM_OBJECT: u16 = 0xDC0E;
    pub const PERSISTENT_UID: u16 = 0xDC41;
    pub const NAME: u16 = 0xDC44;
    pub const DATE_ADDED: u16 = 0xDC4E;
    /// Wildcard "all properties" for GetObjectPropList.
    pub const ALL: u32 = 0xFFFF_FFFF;
}

/// PTP/MTP data type codes.
pub mod datatype {
    pub const UNDEF: u16 = 0x0000;
    pub const INT8: u16 = 0x0001;
    pub const UINT8: u16 = 0x0002;
    pub const INT16: u16 = 0x0003;
    pub const UINT16: u16 = 0x0004;
    pub const INT32: u16 = 0x0005;
    pub const UINT32: u16 = 0x0006;
    pub const INT64: u16 = 0x0007;
    pub const UINT64: u16 = 0x0008;
    pub const INT128: u16 = 0x0009;
    pub const UINT128: u16 = 0x000A;
    /// Array flag: `0x4000 | element type`.
    pub const ARRAY: u16 = 0x4000;
    pub const STR: u16 = 0xFFFF;
}

/// Storage types (StorageInfo.StorageType).
pub mod storage_type {
    pub const FIXED_ROM: u16 = 0x0001;
    pub const REMOVABLE_ROM: u16 = 0x0002;
    pub const FIXED_RAM: u16 = 0x0003;
    pub const REMOVABLE_RAM: u16 = 0x0004;
}

/// Association types.
pub mod association {
    pub const GENERIC_FOLDER: u16 = 0x0001;
}

/// PTP-over-USB still image class requests (bRequest).
pub mod class_request {
    pub const CANCEL: u8 = 0x64;
    pub const GET_EXTENDED_EVENT_DATA: u8 = 0x65;
    pub const DEVICE_RESET: u8 = 0x66;
    pub const GET_DEVICE_STATUS: u8 = 0x67;
}

/// GetObjectHandles / SendObjectInfo parent value meaning "root of the storage".
pub const ROOT_PARENT: u32 = 0xFFFF_FFFF;
/// "All storages" wildcard.
pub const ALL_STORAGES: u32 = 0xFFFF_FFFF;
