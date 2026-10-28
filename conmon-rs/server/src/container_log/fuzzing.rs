use super::{Line, LineBuffer};
use crate::container_io::Pipe;

/// The output of a pipe, read in chunks split at NUL bytes, with a pause
/// after every chunk whose length is a multiple of three. Joining the
/// lines gives back the output, with "\n" for every line ending, the
/// buffer never holds a whole line, and the lines of valid UTF-8 output
/// are valid UTF-8 as well, none got cut inside a character.
pub fn lines(data: &[u8]) {
    let output: Vec<u8> = data.iter().copied().filter(|b| *b != 0).collect();
    let utf8 = str::from_utf8(&output).is_ok();
    let mut buffer = LineBuffer::default();
    let mut joined = vec![];
    let mut join = |lines: Vec<Line>| {
        for line in lines {
            assert!(!utf8 || str::from_utf8(&line.text).is_ok());
            joined.extend_from_slice(&line.text);
            if !line.partial {
                joined.push(b'\n');
            }
        }
    };
    for chunk in data.split(|b| *b == 0) {
        join(buffer.push(Pipe::StdOut, chunk));
        assert!(buffer.stdout.len() < LineBuffer::MAX_LINE);
        if chunk.len() % 3 == 0 {
            join(buffer.flush(Pipe::StdOut));
            assert!(buffer.stdout.len() <= 4);
        }
    }
    join(buffer.finish(Pipe::StdOut));
    assert!(!buffer.has_pending(Pipe::StdOut));

    let mut expected = Vec::with_capacity(output.len());
    let mut rest = output.as_slice();
    while let Some(end) = memchr::memmem::find(rest, b"\r\n") {
        expected.extend_from_slice(&rest[..end]);
        expected.push(b'\n');
        rest = &rest[end + 2..];
    }
    expected.extend_from_slice(rest);
    assert_eq!(joined, expected);
}
