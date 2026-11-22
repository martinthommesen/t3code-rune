use std::fs::File;
use std::io::{self, Seek};
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

use rune::{
    Align, El, FileDialogOptions, FileDialogResult, Icon, TextStyle, Tx, col, el, icon, row, text,
};
use uuid::Uuid;

use crate::connection::file_preview::{AssetOutcome, AssetPurpose};

use super::{CHEVRON, CRUMB, Colors, ERROR, Load, Palette, Preview, State, TAB, control, filename};

const SAVE: &str = "file-preview-save";
const HEADING: TextStyle = TextStyle::new(14.0, 500.0).lh(20.0);
static DOWNLOAD: Icon = Icon {
    name: "download",
    view: 24.0,
    body: r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/></g>"#,
};

pub(super) enum Phase {
    Checking,
    Unavailable(String),
    Ready,
    Preparing,
    Choosing,
    Writing(Receiver<io::Result<()>>),
    SaveError(String),
}

impl State {
    pub(in crate::ui) fn file_preview_asset(
        &mut self,
        id: Uuid,
        result: AssetOutcome,
        tx: &mut Tx,
    ) {
        let Some(preview) = self
            .file_preview
            .as_mut()
            .filter(|preview| preview.id == id)
        else {
            return;
        };
        let Load::Binary(phase) = &mut preview.load else {
            return;
        };
        *phase = match (&*phase, result) {
            (Phase::Checking, AssetOutcome::Available) => Phase::Ready,
            (Phase::Checking, AssetOutcome::Failed(message)) => Phase::Unavailable(message),
            (Phase::Preparing, AssetOutcome::Failed(message)) => Phase::SaveError(message),
            (Phase::Preparing, AssetOutcome::Prepared(file)) => {
                let options = FileDialogOptions {
                    title: "Save file".to_owned(),
                    file_name: Some(filename(&preview.target.path).to_owned()),
                    ..Default::default()
                };
                tx.save_file(options, move |state: &mut State, answer, tx| {
                    state.save_preview_destination(id, file, answer, tx);
                });
                Phase::Choosing
            }
            _ => return,
        };
    }

    fn prepare_preview_save(&mut self, _: &mut Tx) {
        self.request_preview_asset(AssetPurpose::Download);
    }

    fn retry_preview_asset(&mut self, _: &mut Tx) {
        self.request_preview_asset(AssetPurpose::Inspect);
    }

    fn request_preview_asset(&mut self, purpose: AssetPurpose) {
        let Some((preview, connection)) = self.file_preview.as_mut().zip(self.connection.as_ref())
        else {
            return;
        };
        let Load::Binary(phase) = &preview.load else {
            return;
        };
        let next = match (purpose, phase) {
            (AssetPurpose::Download, Phase::Ready | Phase::SaveError(_)) => Phase::Preparing,
            (AssetPurpose::Inspect, Phase::Unavailable(_)) => Phase::Checking,
            _ => return,
        };
        connection.prepare_file_asset(preview.request(), purpose);
        preview.load = Load::Binary(next);
    }

    fn save_preview_destination(
        &mut self,
        id: Uuid,
        file: File,
        answer: FileDialogResult,
        tx: &mut Tx,
    ) {
        let Some(preview) = self
            .file_preview
            .as_mut()
            .filter(|preview| preview.id == id)
        else {
            return;
        };
        if !matches!(preview.load, Load::Binary(Phase::Choosing)) {
            return;
        }
        let phase = match answer {
            Ok(None) => Phase::Ready,
            Err(error) => Phase::SaveError(error.to_string()),
            Ok(Some(destination)) => {
                let (send, receive) = sync_channel(1);
                let wake = self.wake.clone();
                let task = std::thread::Builder::new()
                    .name("save-workspace-preview".into())
                    .spawn(move || {
                        let result = save_to(file, &destination);
                        let _ = send.send(result);
                        if let Some(wake) = wake {
                            wake();
                        }
                    });
                match task {
                    Ok(_) => Phase::Writing(receive),
                    Err(error) => Phase::SaveError(error.to_string()),
                }
            }
        };
        if !matches!(phase, Phase::Writing(_)) {
            tx.focus_when_shown(SAVE, false);
        }
        preview.load = Load::Binary(phase);
    }

    pub(in crate::ui) fn poll_file_preview_save(&mut self, tx: &mut Tx) -> bool {
        let Some(Preview {
            load: Load::Binary(phase),
            ..
        }) = &mut self.file_preview
        else {
            return false;
        };
        let Phase::Writing(result) = phase else {
            return false;
        };
        let complete = match result.try_recv() {
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                Phase::SaveError("Unable to finish writing the file.".into())
            }
            Ok(Err(error)) => Phase::SaveError(error.to_string()),
            Ok(Ok(())) => Phase::Ready,
        };
        *phase = complete;
        tx.focus_when_shown(SAVE, false);
        true
    }
}

fn save_to(mut file: File, path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("The selected destination has no folder."))?;
    file.rewind()?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    io::copy(&mut file, &mut staged)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|failure| failure.error)?;
    Ok(())
}

pub(super) fn view(preview: &Preview, phase: &Phase, colors: Colors, palette: Palette) -> El {
    let name = filename(&preview.target.path);
    let saving = matches!(
        phase,
        Phase::Preparing | Phase::Choosing | Phase::Writing(_)
    );
    let mut toolbar = row()
        .key("file-preview-binary-toolbar")
        .w_full()
        .h(28.0)
        .pl(14.0)
        .pr(12.0)
        .gap(4.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], colors.separator)
        .border_layout()
        .child(text("File").font(CRUMB).color(colors.muted))
        .child(
            icon(&CHEVRON, 14.0)
                .color(colors.muted.alpha(0.6))
                .a11y_hidden(),
        )
        .child(text(name).font(TAB).color(colors.ink).truncate())
        .child(el().grow(1.0));
    if !matches!(phase, Phase::Checking | Phase::Unavailable(_)) {
        toolbar = toolbar.child(
            control(
                SAVE,
                if saving {
                    "Preparing file…"
                } else {
                    "Save file"
                },
                &DOWNLOAD,
                colors,
                palette,
                State::prepare_preview_save,
            )
            .disabled(saving),
        );
    }

    let mut body = col()
        .key("file-preview-binary-body")
        .w_full()
        .min_h(0.0)
        .grow(1.0)
        .px(24.0)
        .center()
        .gap(4.0);
    body = match phase {
        Phase::Checking => body.child(
            text("Loading…")
                .font(CRUMB)
                .color(colors.muted)
                .role("status"),
        ),
        Phase::Unavailable(message) => body
            .child(
                text(message)
                    .font(ERROR)
                    .color(colors.error)
                    .align(Align::Center),
            )
            .child(super::super::button(
                "file-preview-retry-asset",
                "Retry",
                palette,
                false,
                true,
                State::retry_preview_asset,
            )),
        _ => {
            let extension = name
                .rsplit('.')
                .next()
                .filter(|part| !part.is_empty())
                .unwrap_or("this format");
            let body = body
                .child(
                    text("No preview for this file")
                        .font(HEADING)
                        .color(colors.ink),
                )
                .child(
                    text(format!(
                        "Save it to open in an app that supports {extension} files."
                    ))
                    .font(ERROR)
                    .color(colors.muted)
                    .max_w(384.0)
                    .wrap()
                    .align(Align::Center),
                );
            match phase {
                Phase::SaveError(message) => body.child(
                    text(format!("Could not save file. {message}"))
                        .font(ERROR)
                        .color(colors.error)
                        .role("alert")
                        .wrap()
                        .align(Align::Center),
                ),
                _ => body,
            }
        }
    };
    col()
        .w_full()
        .min_h(0.0)
        .grow(1.0)
        .child(toolbar)
        .child(el().h(12.0))
        .child(body)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn saves_the_complete_prepared_binary_from_the_start() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("copy.blob");
        let payload = b"test\0bytes\xff";
        let mut prepared = tempfile::tempfile().unwrap();
        prepared.write_all(payload).unwrap();
        save_to(prepared, &output).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), payload);
    }
}
