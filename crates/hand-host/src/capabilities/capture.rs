use std::collections::VecDeque;

/// The output of a command, kept within a bound: the first half and the last
/// half of what was written, with a note of how much was left out between.
#[derive(Debug)]
pub(crate) struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
    max: usize,
}

impl Capture {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
            max,
        }
    }

    pub(crate) fn push(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len();
        let head = (self.max / 2)
            .saturating_sub(self.head.len())
            .min(bytes.len());
        self.head.extend_from_slice(&bytes[..head]);
        bytes = &bytes[head..];

        let tail = self.max - self.max / 2;
        if bytes.len() >= tail {
            self.tail.clear();
            bytes = &bytes[bytes.len() - tail..];
        }
        let over = (self.tail.len() + bytes.len()).saturating_sub(tail);
        self.tail.drain(..over);
        self.tail.extend(bytes);
    }

    /// The text kept, and whether any was left out.
    pub(crate) fn text(&self) -> (String, bool) {
        let kept = self.head.len() + self.tail.len();
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = String::from_utf8_lossy(&tail);
        let head = String::from_utf8_lossy(&self.head);
        if self.total == kept {
            return (format!("{head}{tail}"), false);
        }
        (
            format!(
                "{head}\n... {} bytes omitted ...\n{tail}",
                self.total - kept
            ),
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_past_its_bound_keeps_its_two_ends_and_says_what_it_left_out() {
        let cases: [(usize, &[&[u8]], &str, bool); 3] = [
            (10, &[b"hello", b" you"], "hello you", false),
            (
                10,
                &[b"0123456789", b"abcdefghij", b"KLMNO"],
                "01234\n... 15 bytes omitted ...\nKLMNO",
                true,
            ),
            (4, &[b"abcdefgh"], "ab\n... 4 bytes omitted ...\ngh", true),
        ];
        for (bound, writes, text, cut) in cases {
            let mut capture = Capture::new(bound);
            for write in writes {
                capture.push(write);
            }
            assert_eq!(capture.text(), (text.into(), cut), "{writes:?}");
        }
    }
}
