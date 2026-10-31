//! The two framings of the browser link, both bounded before anything is
//! allocated for the content.
//!
//! - Native messaging (browser <-> host): a 4 byte little-endian length, then
//!   that many bytes of JSON.
//! - The bridge (host <-> app): one JSON object per line.

use std::io::{self, BufRead, Read, Write};

/// Reads one native messaging frame of at most `max` bytes. `Ok(None)` is a
/// clean end of input (the browser closed the pipe between frames).
///
/// # Errors
/// An I/O error, an input that ends inside a frame, or a frame that declares
/// more than `max` bytes (nothing of it is read or allocated).
pub fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame larger than the limit",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Writes one native messaging frame and flushes.
///
/// # Errors
/// An I/O error, or a body larger than `max`.
pub fn write_frame(w: &mut impl Write, body: &[u8], max: usize) -> io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|_| body.len() <= max)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame larger than the limit"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Reads one line (without the line break) of at most `max` bytes. `Ok(None)`
/// is the end of input; a last line without a break still counts.
///
/// # Errors
/// An I/O error, or a line longer than `max` (reading stops there).
pub fn read_line(r: &mut impl BufRead, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let (take, found) = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or((buf.len(), false), |i| (i, true));
        if line.len() + take > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "line longer than the limit",
            ));
        }
        line.extend_from_slice(&buf[..take]);
        r.consume(take + usize::from(found));
        if found {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{\"a\":1}", 100).unwrap();
        assert_eq!(&buf[..4], &7u32.to_le_bytes());
        let mut r = Cursor::new(buf);
        assert_eq!(read_frame(&mut r, 100).unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(read_frame(&mut r, 100).unwrap(), None, "clean end");
    }

    #[test]
    fn an_oversized_frame_is_refused_without_reading_it() {
        // Declares 4 GiB; only the header exists. A reader that allocated
        // first would fail with out-of-memory, one that read first with EOF.
        let mut r = Cursor::new(u32::MAX.to_le_bytes().to_vec());
        let err = read_frame(&mut r, 1024).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let mut r = Cursor::new(1025u32.to_le_bytes().to_vec());
        assert!(read_frame(&mut r, 1024).is_err());
        let mut r = Cursor::new(1024u32.to_le_bytes().to_vec());
        assert_eq!(
            read_frame(&mut r, 1024).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
            "exactly the limit is allowed, the body is missing here"
        );
    }

    #[test]
    fn a_cut_header_or_body_is_an_error_not_an_end() {
        let mut r = Cursor::new(vec![1, 0]);
        assert!(read_frame(&mut r, 10).is_err());
        let mut r = Cursor::new([5u32.to_le_bytes().as_slice(), b"abc"].concat());
        assert!(read_frame(&mut r, 10).is_err());
    }

    #[test]
    fn writing_refuses_a_body_over_the_limit() {
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &[0; 11], 10).is_err());
        assert!(buf.is_empty(), "nothing is written for a refused frame");
    }

    #[test]
    fn lines_are_bounded() {
        let mut r = Cursor::new(b"one\r\ntwo\nlast".to_vec());
        assert_eq!(read_line(&mut r, 10).unwrap().unwrap(), b"one");
        assert_eq!(read_line(&mut r, 10).unwrap().unwrap(), b"two");
        assert_eq!(read_line(&mut r, 10).unwrap().unwrap(), b"last");
        assert_eq!(read_line(&mut r, 10).unwrap(), None);
        let mut r = Cursor::new(vec![b'x'; 1000]);
        assert!(read_line(&mut r, 100).is_err(), "no newline in sight");
        let mut r = Cursor::new(b"12345\n".to_vec());
        assert!(read_line(&mut r, 4).is_err());
        let mut r = Cursor::new(b"1234\n".to_vec());
        assert!(read_line(&mut r, 4).is_ok());
    }
}
