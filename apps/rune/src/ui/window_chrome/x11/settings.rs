use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window};

use super::super::{CaptionButton, CaptionLayout};

const PROPERTY_LIMIT: u32 = 16_384; // X11 counts four-byte units.
const DECORATION_LAYOUT: &str = "Gtk/DecorationLayout";

pub(super) struct Watcher {
    root: Window,
    selection: Atom,
    property: Atom,
    manager: Atom,
    owner: Window,
}

impl Watcher {
    pub(super) fn install(connection: &impl Connection, root: Window) -> Option<Self> {
        let screen = connection
            .setup()
            .roots
            .iter()
            .position(|screen| screen.root == root)?;
        let atom = |name: &[u8]| Some(connection.intern_atom(false, name).ok()?.reply().ok()?.atom);
        let watcher = Self {
            root,
            selection: atom(format!("_XSETTINGS_S{screen}").as_bytes())?,
            property: atom(b"_XSETTINGS_SETTINGS")?,
            manager: atom(b"MANAGER")?,
            owner: 0,
        };
        watch(connection, root, EventMask::STRUCTURE_NOTIFY)?;
        Some(watcher)
    }

    pub(super) fn changed(&self, event: &Event) -> bool {
        match event {
            Event::ClientMessage(event) if event.format == 32 => {
                event.window == self.root
                    && event.type_ == self.manager
                    && event.data.as_data32()[1] == self.selection
            }
            Event::PropertyNotify(event) => {
                event.window == self.owner && event.atom == self.property
            }
            Event::DestroyNotify(event) => event.window == self.owner,
            _ => false,
        }
    }

    pub(super) fn load(&mut self, connection: &impl Connection) -> CaptionLayout {
        self.read(connection).unwrap_or_default()
    }

    fn read(&mut self, connection: &impl Connection) -> Option<CaptionLayout> {
        let owner = connection
            .get_selection_owner(self.selection)
            .ok()?
            .reply()
            .ok()?
            .owner;
        if owner != self.owner {
            if self.owner != 0 {
                let _ = watch(connection, self.owner, EventMask::NO_EVENT);
            }
            self.owner = owner;
            if owner != 0 {
                watch(
                    connection,
                    owner,
                    EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY,
                )?;
            }
        }
        if owner == 0 {
            return None;
        }
        let property = connection
            .get_property(
                false,
                owner,
                self.property,
                self.property,
                0,
                PROPERTY_LIMIT,
            )
            .ok()?
            .reply()
            .ok()?;
        if property.format == 8 && property.bytes_after == 0 {
            return decode(&property.value);
        }
        None
    }
}

fn watch(connection: &impl Connection, window: Window, mask: EventMask) -> Option<()> {
    connection
        .change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(mask))
        .ok()?
        .check()
        .ok()
}

fn decode(bytes: &[u8]) -> Option<CaptionLayout> {
    let mut buffer = SettingsBuffer {
        little_endian: match bytes.first()? {
            0 => true,
            1 => false,
            _ => return None,
        },
        data: bytes.get(8..)?, // Skip byte order, padding and the settings serial.
    };
    let count = buffer.word()?;
    for _ in 0..count {
        let kind = buffer.take(2)?[0];
        let name_length = buffer.half_word()? as usize;
        let name = buffer.padded(name_length)?;
        buffer.take(4)?; // The last change serial for this setting.
        match kind {
            0 => {
                buffer.take(4)?;
            }
            1 => {
                let length = buffer.word()? as usize;
                let value = buffer.padded(length)?;
                if name == DECORATION_LAYOUT.as_bytes() {
                    return Some(layout(std::str::from_utf8(value).ok()?));
                }
            }
            2 => {
                buffer.take(8)?;
            }
            _ => return None,
        }
    }
    None
}

struct SettingsBuffer<'a> {
    little_endian: bool,
    data: &'a [u8],
}

impl<'a> SettingsBuffer<'a> {
    fn take(&mut self, size: usize) -> Option<&'a [u8]> {
        let (value, rest) = self.data.split_at_checked(size)?;
        self.data = rest;
        Some(value)
    }

    fn padded(&mut self, size: usize) -> Option<&'a [u8]> {
        self.take(size.checked_add(3)? & !3)?.get(..size)
    }

    fn half_word(&mut self) -> Option<u16> {
        let value = self.take(2)?.try_into().ok()?;
        Some(if self.little_endian {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        })
    }

    fn word(&mut self) -> Option<u32> {
        let value = self.take(4)?.try_into().ok()?;
        Some(if self.little_endian {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        })
    }
}

fn layout(value: &str) -> CaptionLayout {
    let (left, right) = value.split_once(':').unwrap_or((value, ""));
    let buttons = |side: &str| {
        side.split(',')
            .filter_map(|value| match value.trim() {
                "minimize" => Some(CaptionButton::Minimize),
                "maximize" => Some(CaptionButton::Maximize),
                "close" => Some(CaptionButton::Close),
                _ => None,
            })
            .collect()
    };
    CaptionLayout {
        leading: buttons(left),
        trailing: buttons(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(value: &str, little_endian: bool) -> Vec<u8> {
        let word = |n: u32| {
            if little_endian {
                n.to_le_bytes()
            } else {
                n.to_be_bytes()
            }
        };
        let mut bytes = vec![u8::from(!little_endian), 0, 0, 0];
        bytes.extend(word(1)); // Serial.
        bytes.extend(word(1)); // Number of settings.
        bytes.extend([1, 0]); // String setting.
        let length = DECORATION_LAYOUT.len() as u16;
        bytes.extend(if little_endian {
            length.to_le_bytes()
        } else {
            length.to_be_bytes()
        });
        bytes.extend(DECORATION_LAYOUT.as_bytes());
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        bytes.extend(word(1)); // Last change serial.
        bytes.extend(word(value.len() as u32));
        bytes.extend(value.as_bytes());
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        bytes
    }

    #[test]
    fn decodes_both_byte_orders_and_preserves_the_desktop_button_positions() {
        for little_endian in [false, true] {
            let wire = packet("close:minimize,menu,maximize", little_endian);
            let layout = decode(&wire).unwrap();
            assert!(matches!(&layout.leading[..], [CaptionButton::Close]));
            assert!(matches!(
                &layout.trailing[..],
                [CaptionButton::Minimize, CaptionButton::Maximize]
            ));
            assert!(decode(&wire[..wire.len() - 1]).is_none());
        }
    }

    #[test]
    fn an_explicit_empty_button_order_overrides_default_buttons() {
        let wire = packet("appmenu:", true);
        let layout = decode(&wire).unwrap();
        assert!(layout.leading.is_empty() && layout.trailing.is_empty());
    }
}
