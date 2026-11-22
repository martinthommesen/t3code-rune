#[derive(Default)]
pub(super) struct TerminalControl {
    pub(super) focus_reporting: bool,
    replies: String,
}

impl TerminalControl {
    pub(super) fn take_replies(&mut self) -> String {
        std::mem::take(&mut self.replies)
    }
}

impl vt100::Callbacks for TerminalControl {
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        if i1 != Some(b'?') || !params.contains(&&[1004][..]) {
            return;
        }
        match (i2, action) {
            (None, 'h') => self.focus_reporting = true,
            (None, 'l') => self.focus_reporting = false,
            (Some(b'$'), 'p') if self.replies.len() < 16_384 => {
                self.replies.push_str(if self.focus_reporting {
                    "\x1b[?1004;1$y"
                } else {
                    "\x1b[?1004;2$y"
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_and_reports_focus_mode_across_output_chunks() {
        let mut screen = vt100::Parser::new_with_callbacks(20, 80, 0, TerminalControl::default());
        screen.process(b"\x1b[?1004$p\x1b[?10");
        assert!(!screen.callbacks().focus_reporting);
        assert_eq!(screen.callbacks_mut().take_replies(), "\x1b[?1004;2$y");
        screen.process(b"04h\x1b[?1004$p");
        assert!(screen.callbacks().focus_reporting);
        assert_eq!(screen.callbacks_mut().take_replies(), "\x1b[?1004;1$y");
        screen.process(b"\x1b[?1004l");
        assert!(!screen.callbacks().focus_reporting);
    }
}
