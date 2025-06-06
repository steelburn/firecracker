// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

use std::fmt::Debug;
use std::io::Write;
use std::num::Wrapping;

use vm_memory::{VolatileMemoryError, VolatileSlice, WriteVolatile};

use super::{VsockCsmError, defs};
use crate::utils::wrap_usize_to_u32;
use crate::vstate::memory::{BitmapSlice, Bytes};

/// A simple ring-buffer implementation, used by vsock connections to buffer TX (guest -> host)
/// data.  Memory for this buffer is allocated lazily, since buffering will only be needed when
/// the host can't read fast enough.
#[derive(Debug)]
pub struct TxBuf {
    /// The actual u8 buffer - only allocated after the first push.
    data: Option<Box<[u8]>>,
    /// Ring-buffer head offset - where new data is pushed to.
    head: Wrapping<u32>,
    /// Ring-buffer tail offset - where data is flushed from.
    tail: Wrapping<u32>,
}

impl TxBuf {
    /// Total buffer size, in bytes.
    const SIZE: usize = defs::CONN_TX_BUF_SIZE as usize;

    /// Ring-buffer constructor.
    pub fn new() -> Self {
        Self {
            data: None,
            head: Wrapping(0),
            tail: Wrapping(0),
        }
    }

    /// Get the used length of this buffer - number of bytes that have been pushed in, but not
    /// yet flushed out.
    pub fn len(&self) -> usize {
        (self.head - self.tail).0 as usize
    }

    /// Push a byte slice onto the ring-buffer.
    ///
    /// Either the entire source slice will be pushed to the ring-buffer, or none of it, if
    /// there isn't enough room, in which case `Err(Error::TxBufFull)` is returned.
    pub fn push(&mut self, src: &VolatileSlice<impl BitmapSlice>) -> Result<(), VsockCsmError> {
        // Error out if there's no room to push the entire slice.
        if self.len() + src.len() > Self::SIZE {
            return Err(VsockCsmError::TxBufFull);
        }

        let data = self
            .data
            .get_or_insert_with(|| vec![0u8; Self::SIZE].into_boxed_slice());

        // Buffer head, as an offset into the data slice.
        let head_ofs = self.head.0 as usize % Self::SIZE;

        // Pushing a slice to this buffer can take either one or two slice copies: - one copy,
        // if the slice fits between `head_ofs` and `Self::SIZE`; or - two copies, if the
        // ring-buffer head wraps around.

        // First copy length: we can only go from the head offset up to the total buffer size.
        let len = std::cmp::min(Self::SIZE - head_ofs, src.len());

        let _ = src.read(&mut data[head_ofs..(head_ofs + len)], 0);

        // If the slice didn't fit, the buffer head will wrap around, and pushing continues
        // from the start of the buffer (`&self.data[0]`).
        if len < src.len() {
            let _ = src.read(&mut data[..(src.len() - len)], len);
        }

        // Either way, we've just pushed exactly `src.len()` bytes, so that's the amount by
        // which the (wrapping) buffer head needs to move forward.
        self.head += wrap_usize_to_u32(src.len());

        Ok(())
    }

    /// Flush the contents of the ring-buffer to a writable stream.
    ///
    /// Return the number of bytes that have been transferred out of the ring-buffer and into
    /// the writable stream.
    pub fn flush_to<W: Write + Debug>(&mut self, sink: &mut W) -> Result<usize, VsockCsmError> {
        // Nothing to do, if this buffer holds no data.
        if self.is_empty() {
            return Ok(0);
        }

        // Buffer tail, as an offset into the buffer data slice.
        let tail_ofs = self.tail.0 as usize % Self::SIZE;

        // Flushing the buffer can take either one or two writes:
        // - one write, if the tail doesn't need to wrap around to reach the head; or
        // - two writes, if the tail would wrap around: tail to slice end, then slice end to head.

        // First write length: the lesser of tail to slice end, or tail to head.
        let len_to_write = std::cmp::min(Self::SIZE - tail_ofs, self.len());

        // It's safe to unwrap here, since we've already checked if the buffer was empty.
        let data = self.data.as_ref().unwrap();

        // Issue the first write and absorb any `WouldBlock` error (we can just try again
        // later).
        let written = sink
            .write(&data[tail_ofs..(tail_ofs + len_to_write)])
            .map_err(VsockCsmError::TxBufFlush)?;

        // Move the buffer tail ahead by the amount (of bytes) we were able to flush out.
        self.tail += wrap_usize_to_u32(written);

        // If we weren't able to flush out as much as we tried, there's no point in attempting
        // our second write.
        if written < len_to_write {
            return Ok(written);
        }

        // Attempt our second write. This will return immediately if a second write isn't
        // needed, since checking for an empty buffer is the first thing we do in this
        // function.
        //
        // Interesting corner case: if we've already written some data in the first pass,
        // and then the second write fails, we will consider the flush action a success
        // and return the number of bytes written in the first pass.
        Ok(written + self.flush_to(sink).unwrap_or(0))
    }

    /// Check if the buffer holds any data that hasn't yet been flushed out.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl WriteVolatile for TxBuf {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        self.push(buf)
            .map(|()| buf.len())
            .map_err(|err| VolatileMemoryError::IOError(std::io::Error::other(err)))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error as IoError, ErrorKind, Write};

    use super::*;

    #[derive(Debug)]
    struct TestSink {
        data: Vec<u8>,
        err: Option<IoError>,
        capacity: usize,
    }

    impl TestSink {
        const DEFAULT_CAPACITY: usize = 2 * TxBuf::SIZE;
        fn new() -> Self {
            Self {
                data: Vec::with_capacity(Self::DEFAULT_CAPACITY),
                err: None,
                capacity: Self::DEFAULT_CAPACITY,
            }
        }
    }

    impl Write for TestSink {
        fn write(&mut self, src: &[u8]) -> Result<usize, IoError> {
            if self.err.is_some() {
                return Err(self.err.take().unwrap());
            }
            let len_to_push = std::cmp::min(self.capacity - self.data.len(), src.len());
            self.data.extend_from_slice(&src[..len_to_push]);
            Ok(len_to_push)
        }
        fn flush(&mut self) -> Result<(), IoError> {
            Ok(())
        }
    }

    impl TestSink {
        fn clear(&mut self) {
            self.data = Vec::with_capacity(self.capacity);
            self.err = None;
        }
        fn set_err(&mut self, err: IoError) {
            self.err = Some(err);
        }
        fn set_capacity(&mut self, capacity: usize) {
            self.capacity = capacity;
            if self.data.len() > self.capacity {
                self.data.resize(self.capacity, 0);
            }
        }
    }

    #[test]
    fn test_push_nowrap() {
        let mut txbuf = TxBuf::new();
        let mut sink = TestSink::new();
        assert!(txbuf.is_empty());

        assert!(txbuf.data.is_none());

        txbuf
            .push(&VolatileSlice::from([1, 2, 3, 4].as_mut_slice()))
            .unwrap();
        txbuf
            .push(&VolatileSlice::from([5, 6, 7, 8].as_mut_slice()))
            .unwrap();
        txbuf.flush_to(&mut sink).unwrap();
        assert_eq!(sink.data, [1, 2, 3, 4, 5, 6, 7, 8]);
        sink.clear();

        txbuf
            .write_all_volatile(&VolatileSlice::from([10, 11, 12, 13].as_mut_slice()))
            .unwrap();
        txbuf
            .write_all_volatile(&VolatileSlice::from([14, 15, 16, 17].as_mut_slice()))
            .unwrap();

        txbuf.flush_to(&mut sink).unwrap();
        assert_eq!(sink.data, [10, 11, 12, 13, 14, 15, 16, 17]);
        sink.clear();
    }

    #[test]
    fn test_push_wrap() {
        let mut txbuf = TxBuf::new();
        let mut sink = TestSink::new();
        let mut tmp: Vec<u8> = vec![0; TxBuf::SIZE - 2];
        txbuf
            .push(&VolatileSlice::from(tmp.as_mut_slice()))
            .unwrap();
        txbuf.flush_to(&mut sink).unwrap();
        sink.clear();

        txbuf
            .push(&VolatileSlice::from([1, 2, 3, 4].as_mut_slice()))
            .unwrap();
        assert_eq!(txbuf.flush_to(&mut sink).unwrap(), 4);
        assert_eq!(sink.data, [1, 2, 3, 4]);

        sink.clear();

        txbuf
            .write_all_volatile(&VolatileSlice::from([5, 6, 7, 8].as_mut_slice()))
            .unwrap();
        assert_eq!(txbuf.flush_to(&mut sink).unwrap(), 4);
        assert_eq!(sink.data, [5, 6, 7, 8]);
    }

    #[test]
    fn test_push_error() {
        let mut txbuf = TxBuf::new();
        let mut tmp = Vec::with_capacity(TxBuf::SIZE);

        tmp.resize(TxBuf::SIZE - 1, 0);
        txbuf
            .push(&VolatileSlice::from(tmp.as_mut_slice()))
            .unwrap();
        match txbuf.push(&VolatileSlice::from([1, 2].as_mut_slice())) {
            Err(VsockCsmError::TxBufFull) => (),
            other => panic!("Unexpected result: {:?}", other),
        }
        match txbuf.write_volatile(&VolatileSlice::from([1, 2].as_mut_slice())) {
            Err(err) => {
                assert_eq!(
                    format!("{}", err),
                    "Attempted to push data to a full TX buffer"
                );
            }
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_incomplete_flush() {
        let mut txbuf = TxBuf::new();
        let mut sink = TestSink::new();

        sink.set_capacity(2);
        txbuf
            .push(&VolatileSlice::from([1, 2, 3, 4].as_mut_slice()))
            .unwrap();
        assert_eq!(txbuf.flush_to(&mut sink).unwrap(), 2);
        assert_eq!(txbuf.len(), 2);
        assert_eq!(sink.data, [1, 2]);

        sink.set_capacity(4);
        assert_eq!(txbuf.flush_to(&mut sink).unwrap(), 2);
        assert!(txbuf.is_empty());
        assert_eq!(sink.data, [1, 2, 3, 4]);
    }

    #[test]
    fn test_flush_error() {
        const EACCESS: i32 = 13;

        let mut txbuf = TxBuf::new();
        let mut sink = TestSink::new();

        txbuf
            .push(&VolatileSlice::from([1, 2, 3, 4].as_mut_slice()))
            .unwrap();
        let io_err = IoError::from_raw_os_error(EACCESS);
        sink.set_err(io_err);
        match txbuf.flush_to(&mut sink) {
            Err(VsockCsmError::TxBufFlush(ref err))
                if err.kind() == ErrorKind::PermissionDenied => {}
            other => panic!("Unexpected result: {:?}", other),
        }
    }
}
