use std::mem::{self, size_of};
use std::{io, ptr};

#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::Input::GetRawInputBuffer;
use windows_sys::Win32::UI::Input::{
    RAWINPUT, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};

/// A batch of raw-input records retained between event-loop dispatch passes.
///
/// Records have variable sizes. Follow dwSize and native NEXTRAWINPUTBLOCK alignment when
/// advancing through the buffer.
pub(crate) struct RawInputBuffer {
    // One KiB of storage with the eight-byte alignment required by the buffered API.
    storage: [u64; 128],
    offset: usize,
    remaining: usize,
}

impl RawInputBuffer {
    pub(crate) fn new() -> Option<Self> {
        // WOW64 uses a different buffered payload offset from native x86 RAWINPUT. Keep
        // all 32-bit builds on GetRawInputData until that format is explicitly supported.
        cfg!(target_pointer_width = "64")
            .then_some(Self { storage: [0; 128], offset: 0, remaining: 0 })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining == 0
    }

    #[cfg(target_pointer_width = "64")]
    pub(crate) fn read(&mut self) -> io::Result<()> {
        // Never overwrite records already consumed from the Windows queue.
        assert!(self.is_empty());
        self.offset = 0;
        let mut size = mem::size_of_val(&self.storage) as u32;
        let count = unsafe {
            GetRawInputBuffer(
                self.storage.as_mut_ptr().cast(),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if count == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        self.remaining = count as usize;
        if self.remaining > mem::size_of_val(&self.storage) / size_of::<RAWINPUTHEADER>() {
            return Err(self.invalid_record());
        }
        Ok(())
    }

    #[cfg(not(target_pointer_width = "64"))]
    pub(crate) fn read(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "buffered raw input requires 64-bit"))
    }

    /// Decode one supported record. Unknown/HID blocks are skipped by their declared size,
    /// matching handle_raw_input's mouse/keyboard-only behavior without misreading a union.
    pub(crate) fn next_record(&mut self) -> io::Result<Option<RAWINPUT>> {
        while !self.is_empty() {
            let capacity = mem::size_of_val(&self.storage);
            if self.offset > capacity.saturating_sub(size_of::<RAWINPUTHEADER>()) {
                return Err(self.invalid_record());
            }

            // SAFETY: The header fits in initialized storage; read_unaligned avoids making
            // a reference to a struct over an opaque byte buffer.
            let record = unsafe { self.storage.as_ptr().cast::<u8>().add(self.offset) };
            let header = unsafe { record.cast::<RAWINPUTHEADER>().read_unaligned() };
            let record_size = header.dwSize as usize;
            if record_size < size_of::<RAWINPUTHEADER>() || record_size > capacity - self.offset {
                return Err(self.invalid_record());
            }

            let payload_size = match header.dwType {
                RIM_TYPEMOUSE => size_of::<RAWMOUSE>(),
                RIM_TYPEKEYBOARD => size_of::<RAWKEYBOARD>(),
                _ => 0,
            };
            let payload_offset = mem::offset_of!(RAWINPUT, data);
            if payload_size != 0 && record_size < payload_offset + payload_size {
                return Err(self.invalid_record());
            }

            // Equivalent to native NEXTRAWINPUTBLOCK: round the end up to pointer alignment.
            let alignment = size_of::<usize>();
            self.offset = (self.offset + record_size + alignment - 1) & !(alignment - 1);
            self.remaining -= 1;

            if payload_size != 0 {
                let mut data = RAWINPUT { header, ..RAWINPUT::default() };
                // SAFETY: The complete mouse/keyboard payload fits in both the validated
                // source record and RAWINPUT's union. All other destination bytes are zeroed.
                unsafe {
                    ptr::copy_nonoverlapping(
                        record.add(payload_offset),
                        &mut data.data as *mut _ as *mut u8,
                        payload_size,
                    );
                }
                return Ok(Some(data));
            }
        }
        Ok(None)
    }

    fn invalid_record(&mut self) -> io::Error {
        self.remaining = 0;
        io::Error::new(io::ErrorKind::InvalidData, "invalid buffered RAWINPUT block")
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use windows_sys::Win32::UI::Input::RIM_TYPEHID;

    use super::*;

    fn append<T: Copy>(buffer: &mut RawInputBuffer, offset: usize, kind: u32, payload: T) -> usize {
        let payload_offset = mem::offset_of!(RAWINPUT, data);
        let size = payload_offset + size_of::<T>();
        let header = RAWINPUTHEADER { dwType: kind, dwSize: size as u32, ..Default::default() };
        assert!(offset + size <= mem::size_of_val(&buffer.storage));
        unsafe {
            let destination = buffer.storage.as_mut_ptr().cast::<u8>().add(offset);
            destination.cast::<RAWINPUTHEADER>().write_unaligned(header);
            destination.add(payload_offset).cast::<T>().write_unaligned(payload);
        }
        buffer.remaining += 1;
        let alignment = size_of::<usize>();
        (offset + size + alignment - 1) & !(alignment - 1)
    }

    #[test]
    fn walks_mixed_record_sizes() {
        let mut buffer = RawInputBuffer::new().unwrap();
        let end = append(
            &mut buffer,
            0,
            RIM_TYPEKEYBOARD,
            RAWKEYBOARD { VKey: 0x41, ..Default::default() },
        );
        append(
            &mut buffer,
            end,
            RIM_TYPEMOUSE,
            RAWMOUSE { lLastX: 12, lLastY: -7, ..Default::default() },
        );

        let keyboard = buffer.next_record().unwrap().unwrap();
        assert_eq!(keyboard.header.dwType, RIM_TYPEKEYBOARD);
        assert_eq!(unsafe { keyboard.data.keyboard.VKey }, 0x41);
        let mouse = buffer.next_record().unwrap().unwrap();
        assert_eq!(mouse.header.dwType, RIM_TYPEMOUSE);
        assert_eq!(unsafe { mouse.data.mouse.lLastX }, 12);
        assert_eq!(unsafe { mouse.data.mouse.lLastY }, -7);
        assert!(buffer.next_record().unwrap().is_none());
    }

    #[test]
    fn retains_unread_records_between_calls() {
        let mut buffer = RawInputBuffer::new().unwrap();
        let end = append(&mut buffer, 0, RIM_TYPEMOUSE, RAWMOUSE::default());
        append(&mut buffer, end, RIM_TYPEMOUSE, RAWMOUSE::default());
        assert!(buffer.next_record().unwrap().is_some());
        assert!(!buffer.is_empty());
        assert!(buffer.next_record().unwrap().is_some());
        assert!(buffer.is_empty());
    }

    #[test]
    fn skips_unknown_blocks_and_their_alignment_padding() {
        let mut buffer = RawInputBuffer::new().unwrap();
        let end = append(&mut buffer, 0, RIM_TYPEHID, [0u8; 13]);
        append(
            &mut buffer,
            end,
            RIM_TYPEMOUSE,
            RAWMOUSE { lLastX: 5, ..Default::default() },
        );
        let mouse = buffer.next_record().unwrap().unwrap();
        assert_eq!(mouse.header.dwType, RIM_TYPEMOUSE);
        assert_eq!(unsafe { mouse.data.mouse.lLastX }, 5);
        assert!(buffer.is_empty());
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut buffer = RawInputBuffer::new().unwrap();
        append(&mut buffer, 0, RIM_TYPEMOUSE, [0u8; 1]);
        assert!(buffer.next_record().is_err());
        assert!(buffer.is_empty());
    }

    #[test]
    fn rejects_out_of_bounds_record() {
        let mut buffer = RawInputBuffer::new().unwrap();
        append(&mut buffer, 0, RIM_TYPEMOUSE, RAWMOUSE::default());
        unsafe {
            (*buffer.storage.as_mut_ptr().cast::<RAWINPUTHEADER>()).dwSize = u32::MAX;
        }
        assert!(buffer.next_record().is_err());
    }

    #[test]
    fn rejects_missing_header() {
        let mut buffer = RawInputBuffer::new().unwrap();
        buffer.offset = mem::size_of_val(&buffer.storage) - 1;
        buffer.remaining = 1;
        assert!(buffer.next_record().is_err());
    }
}
