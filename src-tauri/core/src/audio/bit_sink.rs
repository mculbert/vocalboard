//! Bit sink: a flacenc [`BitSink`] that writes encoded bits directly to a [`Write`] impl,
//! enabling O(one block) streaming FLAC output (no whole-stream `ByteSink` buffer).
//!
//! [`WriteBitSink`] implements flacenc's [`BitSink`] against any `W: Write`. It buffers
//! partial bits into a single `u8` accumulator and flushes complete bytes on demand.
//!
//! Bit ordering matches flacenc's [`ByteSink`](flacenc::bitsink::ByteSink): bits within each
//! byte are stored MSB-first, as required by the FLAC container format.

use std::io::{self, Write};

use flacenc::bitsink::{BitSink, Bits};

/// A [`BitSink`] that accumulates bits into a `u8` buffer and flushes complete bytes to `W`.
///
/// Data in `cur_byte` occupies the top `bits_filled` bits; the remaining bits are always zero.
/// After the last frame, call [`align_to_byte`](BitSink::align_to_byte) before seeking back to
/// write the backpatched STREAMINFO.
pub(super) struct WriteBitSink<W: Write> {
    writer: W,
    /// Partial byte being accumulated; data in top `bits_filled` bits, zeros in the rest.
    cur_byte: u8,
    bits_filled: u8,
}

impl<W: Write> WriteBitSink<W> {
    pub(super) fn new(writer: W) -> Self {
        Self {
            writer,
            cur_byte: 0,
            bits_filled: 0,
        }
    }

    /// Consume the sink and return the underlying writer.
    ///
    /// # Panics (debug)
    ///
    /// Panics if unwritten partial bits remain. Call
    /// [`align_to_byte`](BitSink::align_to_byte) before calling this method.
    pub(super) fn into_inner(self) -> W {
        debug_assert_eq!(self.bits_filled, 0, "into_inner called with unaligned bits");
        self.writer
    }

    /// Write `n` bits held in the *top* `n` positions of `val` (lower `64 − n` bits are zero).
    fn write_top_n(&mut self, mut val: u64, mut n: usize) -> io::Result<()> {
        if n == 0 {
            return Ok(());
        }

        // If there is a partial byte in progress, fill as many of its empty slots as possible.
        let room = (8 - self.bits_filled) as usize;
        if room < 8 {
            let take = room.min(n);
            // Top `take` bits of `val` as an integer in [0, 2^take).
            let b = (val >> (64 - take)) as u8;
            // OR them into positions [room-1 .. room-take] of `cur_byte`.
            self.cur_byte |= b << (room - take);
            val <<= take;
            n -= take;
            self.bits_filled += take as u8;
            if self.bits_filled == 8 {
                self.writer.write_all(&[self.cur_byte])?;
                self.cur_byte = 0;
                self.bits_filled = 0;
            }
            if n == 0 {
                return Ok(());
            }
        }

        // Byte-boundary; flush complete bytes directly.
        while n >= 8 {
            self.writer.write_all(&[(val >> 56) as u8])?;
            val <<= 8;
            n -= 8;
        }

        // Store remaining partial bits in the accumulator (lower unused bits stay zero).
        if n > 0 {
            // `val >> 56` brings the top `n` data bits into positions [7 .. 8−n]; bits below are 0.
            self.cur_byte = (val >> 56) as u8;
            self.bits_filled = n as u8;
        }
        Ok(())
    }
}

impl<W: Write> BitSink for WriteBitSink<W> {
    type Error = io::Error;

    fn align_to_byte(&mut self) -> Result<usize, Self::Error> {
        if self.bits_filled == 0 {
            return Ok(0);
        }
        let pad = (8 - self.bits_filled) as usize;
        // `cur_byte` already has zeros in the padding positions.
        self.writer.write_all(&[self.cur_byte])?;
        self.cur_byte = 0;
        self.bits_filled = 0;
        Ok(pad)
    }

    fn write_msbs<T: Bits>(&mut self, val: T, n: usize) -> Result<(), Self::Error> {
        if n == 0 {
            return Ok(());
        }
        let bits = std::mem::size_of::<T>() * 8;
        let val_u64: u64 = val.into();
        // Keep the top `n` bits of the `bits`-bit value, then shift them to the top of u64.
        let bits_mask = if bits < 64 {
            (1u64 << bits) - 1
        } else {
            u64::MAX
        };
        let lower_mask = if bits > n {
            (1u64 << (bits - n)) - 1
        } else {
            0
        };
        let masked = val_u64 & bits_mask & !lower_mask;
        let top = if bits < 64 {
            masked << (64 - bits)
        } else {
            masked
        };
        self.write_top_n(top, n)
    }

    fn write_lsbs<T: Bits>(&mut self, val: T, n: usize) -> Result<(), Self::Error> {
        if n == 0 {
            return Ok(());
        }
        let val_u64: u64 = val.into();
        // Keep the bottom `n` bits, then shift them to the top of u64.
        let lower_mask = if n < 64 { (1u64 << n) - 1 } else { u64::MAX };
        let masked = val_u64 & lower_mask;
        let top = if n < 64 { masked << (64 - n) } else { masked };
        self.write_top_n(top, n)
    }

    fn write<T: Bits>(&mut self, val: T) -> Result<(), Self::Error> {
        let bits = std::mem::size_of::<T>() * 8;
        let val_u64: u64 = val.into();
        let top = if bits < 64 {
            val_u64 << (64 - bits)
        } else {
            val_u64
        };
        self.write_top_n(top, bits)
    }

    // Override to bypass the bit accumulator when already byte-aligned.
    fn write_bytes_aligned(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        let pad = self.align_to_byte()?;
        self.writer.write_all(bytes)?;
        Ok(pad)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use flacenc::bitsink::{BitSink, ByteSink};

    use super::WriteBitSink;

    /// Write a sequence to `WriteBitSink<Vec<u8>>`, align to byte, and return the bytes.
    fn sink_bytes(f: impl FnOnce(&mut WriteBitSink<Vec<u8>>) -> io::Result<()>) -> Vec<u8> {
        let mut sink = WriteBitSink::new(Vec::<u8>::new());
        f(&mut sink).unwrap();
        sink.align_to_byte().unwrap();
        sink.into_inner()
    }

    /// Same sequence through flacenc `ByteSink`; align and return bytes.
    fn byte_sink_bytes(
        f: impl FnOnce(&mut ByteSink) -> Result<(), std::convert::Infallible>,
    ) -> Vec<u8> {
        let mut sink = ByteSink::new();
        f(&mut sink).unwrap();
        sink.align_to_byte().unwrap();
        sink.into_inner()
    }

    // SW9 — WriteBitSink bit-order/alignment parity vs flacenc ByteSink (to_bitstring cases).
    #[test]
    fn sw9_bit_order_matches_byte_sink() {
        // Case 1: write_msbs, partial byte only.
        let a = sink_bytes(|s| s.write_msbs(0xF0u8, 3));
        let b = byte_sink_bytes(|s| s.write_msbs(0xF0u8, 3));
        assert_eq!(a, b, "SW9 case 1: write_msbs(0xF0, 3)");

        // Case 2: write_msbs + write (from bitsink.rs doctest).
        // Expected: "11101010_10101010_101*****" → bytes [0xEA, 0xAA, 0xA0].
        let a = sink_bytes(|s| {
            s.write_msbs(0xF0u8, 3)?;
            s.write(0x5555u16)
        });
        let b = byte_sink_bytes(|s| {
            s.write_msbs(0xF0u8, 3)?;
            s.write(0x5555u16)
        });
        assert_eq!(a, b, "SW9 case 2: write_msbs(0xF0,3) + write(0x5555)");

        // Case 3: write_lsbs sequence (from bytevec_write_lsb test).
        // Expected: "11100000_00000001_11111111_00000000".
        let a = sink_bytes(|s| {
            s.write_lsbs(0xFFu8, 3)?;
            s.write_lsbs(0x0u64, 12)?;
            s.write_lsbs(0xFFFF_FFFFu32, 9)?;
            s.write_lsbs(0x0u16, 8)
        });
        let b = byte_sink_bytes(|s| {
            s.write_lsbs(0xFFu8, 3)?;
            s.write_lsbs(0x0u64, 12)?;
            s.write_lsbs(0xFFFF_FFFFu32, 9)?;
            s.write_lsbs(0x0u16, 8)
        });
        assert_eq!(a, b, "SW9 case 3: write_lsbs sequence");

        // Case 4: write_bytes_aligned after partial byte.
        let a = sink_bytes(|s| {
            s.write_msbs(0u8, 3)?;
            s.write_bytes_aligned(&[0xDE, 0xAD, 0xBE, 0xEF]).map(|_| ())
        });
        let b = byte_sink_bytes(|s| {
            s.write_msbs(0u8, 3)?;
            s.write_bytes_aligned(&[0xDE, 0xAD, 0xBE, 0xEF]).map(|_| ())
        });
        assert_eq!(a, b, "SW9 case 4: write_bytes_aligned");

        // Case 5: full u32 write.
        let a = sink_bytes(|s| s.write(0xDEAD_BEEFu32));
        let b = byte_sink_bytes(|s| s.write(0xDEAD_BEEFu32));
        assert_eq!(a, b, "SW9 case 5: write(u32)");

        // Case 6: align_to_byte returns the correct padding count.
        let mut sink = WriteBitSink::new(Vec::<u8>::new());
        sink.write_msbs(0xFFu8, 5).unwrap();
        let pad = sink.align_to_byte().unwrap();
        assert_eq!(pad, 3, "SW9 case 6: align_to_byte padding");
        let pad2 = sink.align_to_byte().unwrap();
        assert_eq!(pad2, 0, "SW9 case 6: align_to_byte when already aligned");

        // Case 7: full 64-bit values exercise the `bits == 64` / `n >= 64` else-branches in
        // write / write_msbs / write_lsbs (no shift-by-64 UB; `top == masked` passthrough).
        let v = 0xDEAD_BEEF_CAFE_0000u64;
        let a = sink_bytes(|s| s.write(v));
        let b = byte_sink_bytes(|s| s.write(v));
        assert_eq!(a, b, "SW9 case 7: write(u64)");

        let a = sink_bytes(|s| s.write_msbs(v, 64));
        let b = byte_sink_bytes(|s| s.write_msbs(v, 64));
        assert_eq!(a, b, "SW9 case 7: write_msbs(u64, 64)");

        let a = sink_bytes(|s| s.write_lsbs(v, 64));
        let b = byte_sink_bytes(|s| s.write_lsbs(v, 64));
        assert_eq!(a, b, "SW9 case 7: write_lsbs(u64, 64)");
    }

    // SW9b — exhaustive small-width parity vs flacenc ByteSink across every starting
    // alignment (0..8 leading bits) for write / write_msbs / write_lsbs. Drives out
    // off-by-one mutants in `write_top_n`'s room/`n` comparisons and in the bit-width
    // guards of write_msbs/write_lsbs/write that the original SW9 fixed cases missed.
    #[test]
    fn sw9b_exhaustive_alignment_parity() {
        // A grab-bag of writes whose values have bits set both inside and *outside* the
        // width being emitted, so that any dropped mask or wrong shift changes the output.
        type Op = fn(&mut WriteBitSink<Vec<u8>>) -> io::Result<()>;
        type OpB = fn(&mut ByteSink) -> Result<(), std::convert::Infallible>;
        let ours: &[Op] = &[
            |s| s.write(0xB7u8),
            |s| s.write(0x1234u16),
            |s| s.write(0xDEAD_BEEFu32),
            // values with the high bits set so a missing high-byte differs
            |s| s.write_msbs(0xABu8, 5),
            |s| s.write_msbs(0xFEDCu16, 11),
            |s| s.write_msbs(0xDEAD_BEEFu32, 17),
            // write_msbs where n == bit-width (exercises bits > n / bits < 64 guards)
            |s| s.write_msbs(0xC3u8, 8),
            // write_lsbs with values whose *upper* bits are set, so a missing low-mask leaks
            |s| s.write_lsbs(0xABu8, 5),
            |s| s.write_lsbs(0xFEDCu16, 11),
            |s| s.write_lsbs(0xDEAD_BEEFu32, 17),
            |s| s.write_lsbs(0xFFu8, 8),
        ];
        let theirs: &[OpB] = &[
            |s| s.write(0xB7u8),
            |s| s.write(0x1234u16),
            |s| s.write(0xDEAD_BEEFu32),
            |s| s.write_msbs(0xABu8, 5),
            |s| s.write_msbs(0xFEDCu16, 11),
            |s| s.write_msbs(0xDEAD_BEEFu32, 17),
            |s| s.write_msbs(0xC3u8, 8),
            |s| s.write_lsbs(0xABu8, 5),
            |s| s.write_lsbs(0xFEDCu16, 11),
            |s| s.write_lsbs(0xDEAD_BEEFu32, 17),
            |s| s.write_lsbs(0xFFu8, 8),
        ];

        // For each starting alignment (number of leading bits already written), and each op,
        // assert byte-for-byte parity with ByteSink.
        for lead in 0u32..8 {
            for (idx, (op, opb)) in ours.iter().zip(theirs.iter()).enumerate() {
                let a = sink_bytes(|s| {
                    if lead > 0 {
                        s.write_msbs(0u8, lead as usize)?;
                    }
                    op(s)
                });
                let b = byte_sink_bytes(|s| {
                    if lead > 0 {
                        s.write_msbs(0u8, lead as usize)?;
                    }
                    opb(s)
                });
                assert_eq!(a, b, "SW9b: lead={lead}, op#{idx}");
            }
        }
    }

    // SW10 — io::Error from the underlying writer propagates out of WriteBitSink, no panic.
    #[test]
    fn sw10_io_error_propagates() {
        struct FailWriter;
        impl io::Write for FailWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut sink = WriteBitSink::new(FailWriter);
        // 32-bit write forces a flush (complete bytes) → FailWriter returns BrokenPipe.
        let result = sink.write(0xDEAD_BEEFu32);
        assert!(result.is_err(), "SW10: expected Err from FailWriter");
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe,
            "SW10: error kind must be BrokenPipe"
        );
    }
}
