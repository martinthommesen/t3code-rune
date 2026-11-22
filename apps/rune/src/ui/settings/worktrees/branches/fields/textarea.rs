use rune::{Cursor, Drag, El, Icon, Phase, col, icon};

use crate::connection::BranchTextField;

use super::{Palette, State};

const MIN_HEIGHT: f32 = 90.0;
const MAX_HEIGHT: f32 = 256.0;

static GRIP: Icon = Icon {
    name: "branch-instructions-resize",
    view: 12.0,
    body: r#"<path d="m3 10 7-7m-3 7 3-3" fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round"/>"#,
};

#[derive(Default)]
pub(super) struct Resize {
    measured: f32,
    height: Option<f32>,
    preview: Option<f32>,
}

impl Resize {
    fn drag(&mut self, drag: &mut Drag) {
        match drag.phase {
            Phase::Down => drag.data = self.measured.clamp(MIN_HEIGHT, MAX_HEIGHT),
            Phase::Move => {
                self.preview = Some((drag.data + drag.dy).round().clamp(MIN_HEIGHT, MAX_HEIGHT));
            }
            Phase::Up => {
                if let Some(preview) = self.preview.take()
                    && !drag.cancelled()
                    && preview != drag.data
                {
                    self.height = Some(preview);
                }
            }
        }
    }
}

pub(super) fn view(state: &State, input: El, palette: Palette, writable: bool) -> El {
    const FIELD: BranchTextField = BranchTextField::Instructions;
    let resize = &state.settings.branch_fields.editors[FIELD.index()].resize;
    let input = input
        .w_full()
        .min_h(MIN_HEIGHT)
        .max_h(MAX_HEIGHT)
        .border_layout();
    let input = match resize.preview.or(resize.height) {
        Some(height) => input.h(height),
        None => input,
    };
    let mut container = col()
        .key("branch-instructions-textarea")
        .w_full()
        .on_layout(|state: &mut State, [_, _, _, height]| {
            state.settings.branch_fields.editor(FIELD).resize.measured = height;
        })
        .child(input);
    if writable {
        container = container.child(
            col()
                .key("resize-branch-instructions")
                .pin(None, None, Some(1.0), Some(1.0))
                .size(14.0, 14.0)
                .center()
                .cursor(Cursor::ResizeRow)
                .preserve_focus_on_press(true)
                .on_drag(|state: &mut State, drag, tx| {
                    state.settings.branch_fields.editor(FIELD).resize.drag(drag);
                    tx.instant();
                })
                .child(icon(&GRIP, 11.0).color(palette.muted).a11y_hidden()),
        );
    }
    container
}
