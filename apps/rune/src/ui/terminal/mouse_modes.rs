use std::collections::VecDeque;

use alacritty_terminal::vte::{Params, Parser, Perform};

const MODE_CODES: [u16; 8] = [9, 1000, 1002, 1003, 1005, 1006, 1015, 1016];
const MAX_PENDING_QUERIES: usize = 2_048;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Tracking {
    #[default]
    Disabled,
    X10,
    Buttons,
    Drag,
    Motion,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Encoding {
    #[default]
    Classic,
    Utf8,
    Sgr,
    Urxvt,
    Pixels,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Protocol {
    pub(super) tracking: Tracking,
    pub(super) encoding: Encoding,
}

#[derive(Default)]
pub(super) struct MouseModes {
    parser: Parser,
    state: ModeState,
}

#[derive(Default)]
struct ModeState {
    protocol: Protocol,
    enabled: u8,
    queries: VecDeque<ModeQuery>,
}

struct ModeQuery {
    mode: u16,
    enabled: bool,
}

fn mode_bit(mode: u16) -> Option<u8> {
    MODE_CODES
        .iter()
        .position(|&value| value == mode)
        .map(|bit| 1 << bit)
}

impl MouseModes {
    pub(super) fn process(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.state, data);
    }

    pub(super) fn protocol(&self) -> Protocol {
        self.state.protocol
    }

    pub(super) fn rewrite_replies(&mut self, replies: &mut String, waiting: bool) {
        let mut offset = 0;
        while let Some(query) = self.state.queries.front() {
            let prefix = format!("\x1b[?{};", query.mode);
            let marker = replies[offset..]
                .find(&prefix)
                .map(|at| offset + at + prefix.len());
            if let Some(marker) = marker
                && replies.get(marker + 1..marker + 3) == Some("$y")
            {
                replies.replace_range(marker..marker + 1, if query.enabled { "1" } else { "2" });
                offset = marker + 3;
            } else if waiting {
                break;
            }
            self.state.queries.pop_front();
        }
    }
}

impl Perform for ModeState {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if intermediates == b"?$" && action == 'p' {
            for values in params.iter() {
                if let &[mode] = values
                    && let Some(bit) = mode_bit(mode)
                    && self.queries.len() < MAX_PENDING_QUERIES
                {
                    self.queries.push_back(ModeQuery {
                        mode,
                        enabled: self.enabled & bit != 0,
                    });
                }
            }
            return;
        }
        if intermediates != b"?" {
            return;
        }
        let setting = match action {
            'h' => true,
            'l' => false,
            _ => return,
        };
        for values in params.iter() {
            let &[mode] = values else { continue };
            if let Some(bit) = mode_bit(mode) {
                if setting {
                    self.enabled |= bit;
                } else {
                    self.enabled &= !bit;
                }
            }
            match mode {
                9 => {
                    self.protocol.tracking = if setting {
                        Tracking::X10
                    } else {
                        Tracking::Disabled
                    }
                }
                1000 => {
                    self.protocol.tracking = if setting {
                        Tracking::Buttons
                    } else {
                        Tracking::Disabled
                    }
                }
                1002 => {
                    self.protocol.tracking = if setting {
                        Tracking::Drag
                    } else {
                        Tracking::Disabled
                    }
                }
                1003 => {
                    self.protocol.tracking = if setting {
                        Tracking::Motion
                    } else {
                        Tracking::Disabled
                    }
                }
                1005 => {
                    self.protocol.encoding = if setting {
                        Encoding::Utf8
                    } else {
                        Encoding::Classic
                    }
                }
                1006 => {
                    self.protocol.encoding = if setting {
                        Encoding::Sgr
                    } else {
                        Encoding::Classic
                    }
                }
                1015 => {
                    self.protocol.encoding = if setting {
                        Encoding::Urxvt
                    } else {
                        Encoding::Classic
                    }
                }
                1016 => {
                    self.protocol.encoding = if setting {
                        Encoding::Pixels
                    } else {
                        Encoding::Classic
                    }
                }
                _ => {}
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.protocol = Protocol::default();
            self.enabled = 0;
        }
    }
}
