use super::*;

fn collect(lines: Vec<Line>) -> Vec<(String, bool)> {
    lines
        .into_iter()
        .map(|line| {
            (
                String::from_utf8_lossy(&line.text).into_owned(),
                line.partial,
            )
        })
        .collect()
}

#[test]
fn line_buffer_joins_chunks_per_pipe() {
    let mut buffer = LineBuffer::default();
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"first\nab")),
        vec![("first".into(), false)]
    );
    assert!(buffer.has_pending(Pipe::StdOut));
    // The other pipe has its own tail.
    assert!(!buffer.has_pending(Pipe::StdErr));
    assert_eq!(
        collect(buffer.push(Pipe::StdErr, b"err\n")),
        vec![("err".into(), false)]
    );
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"cd\nthird")),
        vec![("abcd".into(), false)]
    );

    // The output ended, the rest is a partial line.
    assert_eq!(
        collect(buffer.finish(Pipe::StdOut)),
        vec![("third".into(), true)]
    );
    assert!(buffer.finish(Pipe::StdOut).is_empty());
    assert!(!buffer.has_pending(Pipe::StdOut));
}

#[test]
fn line_buffer_flushes_paused_output() {
    let mut buffer = LineBuffer::default();
    assert!(buffer.push(Pipe::StdOut, b"prompt> ").is_empty());
    assert_eq!(
        collect(buffer.flush(Pipe::StdOut)),
        vec![("prompt> ".into(), true)]
    );
    assert!(buffer.flush(Pipe::StdOut).is_empty());
    // The continuation is its own line.
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"ls\n")),
        vec![("ls".into(), false)]
    );

    // A pause inside a character or before a "\n" keeps them for the
    // next chunk.
    let euro = "€".as_bytes();
    assert!(
        buffer
            .push(Pipe::StdOut, &[b"a", &euro[..1]].concat())
            .is_empty()
    );
    assert_eq!(
        collect(buffer.flush(Pipe::StdOut)),
        vec![("a".into(), true)]
    );
    assert!(buffer.flush(Pipe::StdOut).is_empty());
    assert!(buffer.has_pending(Pipe::StdOut));
    assert!(
        buffer
            .push(Pipe::StdOut, &[&euro[1..], b"\r"].concat())
            .is_empty()
    );
    assert_eq!(
        collect(buffer.flush(Pipe::StdOut)),
        vec![("€".into(), true)]
    );
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"\n")),
        vec![("".into(), false)]
    );

    // Once the output ended, everything goes.
    assert!(buffer.push(Pipe::StdOut, &euro[..1]).is_empty());
    assert!(buffer.flush(Pipe::StdOut).is_empty());
    assert_eq!(buffer.finish(Pipe::StdOut).len(), 1);
    assert!(!buffer.has_pending(Pipe::StdOut));
}

#[test]
fn line_buffer_handles_split_line_endings_and_characters() {
    let mut buffer = LineBuffer::default();
    // "\r\n" split between two chunks.
    assert!(buffer.push(Pipe::StdOut, b"terminal\r").is_empty());
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"\n")),
        vec![("terminal".into(), false)]
    );

    // A multi byte character split between two chunks.
    let euro = "€".as_bytes();
    assert!(buffer.push(Pipe::StdOut, &euro[..1]).is_empty());
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, &[&euro[1..], b"\n"].concat())),
        vec![("€".into(), false)]
    );
}

#[test]
fn line_buffer_borrows_whole_lines_of_the_chunk() {
    let mut buffer = LineBuffer::default();
    let chunk = b"first\r\nsecond\nthi";
    let lines = buffer.push(Pipe::StdOut, chunk);
    assert_eq!(
        lines,
        [
            Line {
                text: Text::Chunk(&chunk[..5]),
                partial: false,
            },
            Line {
                text: Text::Chunk(&chunk[7..13]),
                partial: false,
            },
        ]
    );
    assert!(
        lines
            .iter()
            .all(|line| chunk.as_ptr_range().contains(&line.text.as_ptr()))
    );

    // The line which started in the buffer gets completed there, the ones
    // after it borrow the chunk again.
    let chunk = b"rd\nfourth\nfif";
    let lines = buffer.push(Pipe::StdOut, chunk);
    assert!(matches!(&lines[0].text, Text::Buffered(text) if text == "third"));
    assert_eq!(lines[1].text, Text::Chunk(b"fourth"));
    assert!(chunk.as_ptr_range().contains(&lines[1].text.as_ptr()));
    assert_eq!(lines.len(), 2);
    assert_eq!(&buffer.stdout[..], b"fif");

    // Without an ending, the chunk only adds to the buffer.
    assert!(buffer.push(Pipe::StdOut, b"th").is_empty());
    assert_eq!(&buffer.stdout[..], b"fifth");
}

#[test]
fn line_buffer_cuts_long_lines() {
    let euro = "€".as_bytes();
    let long = vec![b'a'; LineBuffer::MAX_LINE - 1];

    // An unfinished character at the cut stays buffered.
    let mut buffer = LineBuffer::default();
    let chunk = [long.as_slice(), &euro[..1]].concat();
    let out = buffer.push(Pipe::StdOut, &chunk);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 1);
    assert!(out[0].partial);
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, &[&euro[1..], b"\n"].concat())),
        vec![("€".into(), false)]
    );

    // So does a "\r" which may be followed by its "\n".
    let mut buffer = LineBuffer::default();
    let chunk = [long.as_slice(), b"\r"].concat();
    let out = buffer.push(Pipe::StdOut, &chunk);
    assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 1);
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"\n")),
        vec![("".into(), false)]
    );

    // An invalid byte in front does not cut an unfinished character.
    let mut buffer = LineBuffer::default();
    let mut line = vec![b'a'; LineBuffer::MAX_LINE - 4];
    line.extend_from_slice(&[0xff, b'a', euro[0], euro[1]]);
    let out = buffer.push(Pipe::StdOut, &line);
    assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 2);
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, &[&euro[2..], b"\n"].concat())),
        vec![("€".into(), false)]
    );

    // Invalid bytes are not held back, the buffer would grow forever.
    let mut buffer = LineBuffer::default();
    let mut invalid = vec![0xff; LineBuffer::MAX_LINE];
    invalid.push(0xfe);
    let out = buffer.push(Pipe::StdOut, &invalid);
    assert_eq!(out[0].text.len(), invalid.len());
    assert!(!buffer.has_pending(Pipe::StdOut));
    assert!(buffer.stdout.capacity() <= READ_BUFFER_SIZE);
}

/// The buffer of a line longer than a read chunk gets released once drained,
/// an idle container keeps at most a chunk of it.
#[test]
fn line_buffer_shrinks_after_long_lines() {
    for capacity in [0, 1, READ_BUFFER_SIZE] {
        let mut tail = BytesMut::with_capacity(capacity);
        LineBuffer::shrink(&mut tail);
        assert_eq!(tail.capacity(), capacity);
    }
    let mut tail = BytesMut::with_capacity(READ_BUFFER_SIZE + 1);
    LineBuffer::shrink(&mut tail);
    assert_eq!(tail.capacity(), 0);
    // Not while a line waits for its ending.
    let mut tail = BytesMut::with_capacity(2 * READ_BUFFER_SIZE);
    tail.extend_from_slice(b"line");
    LineBuffer::shrink(&mut tail);
    assert_eq!(tail.capacity(), 2 * READ_BUFFER_SIZE);

    // A short line in a buffer which grew for a long one before, completed,
    // cut or finished, leaves the buffer behind, which gets released.
    let ends: [&dyn Fn(&mut LineBuffer) -> usize; 3] = [
        &|buffer| buffer.push(Pipe::StdOut, b"\n").len(),
        &|buffer| buffer.finish(Pipe::StdOut).len(),
        &|buffer| buffer.flush(Pipe::StdOut).len(),
    ];
    for end in ends {
        let mut buffer = LineBuffer {
            stdout: BytesMut::with_capacity(2 * READ_BUFFER_SIZE),
            ..LineBuffer::default()
        };
        assert!(buffer.push(Pipe::StdOut, b"short").is_empty());
        assert!(buffer.stdout.capacity() >= 2 * READ_BUFFER_SIZE);
        assert_eq!(end(&mut buffer), 1);
        assert_eq!(buffer.stdout.capacity(), 0);
    }
}
