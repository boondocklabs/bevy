use std::{io, path::PathBuf};

use bevy_ecs::entity::Entity;
use bevy_platform::collections::HashMap;
use bevy_window::{FileDragAndDrop, WindowEvent as BevyWindowEvent};
#[cfg(any(unix, windows, target_os = "redox"))]
use url::Url;
use winit::{
    data_transfer::{DataTransferId, TypeHint},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, AsyncRequestSerial, DndAction},
};

use crate::AppSendEvent;

#[derive(Default)]
struct FileTransfer {
    request: Option<AsyncRequestSerial>,
    paths: Vec<PathBuf>,
    dropped: bool,
}

#[derive(Default)]
pub(crate) struct FileDragAndDropState {
    transfers: HashMap<(Entity, DataTransferId), FileTransfer>,
}

impl FileDragAndDropState {
    pub(crate) fn handle(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window: Entity,
        event: &WindowEvent,
        events: &mut Vec<BevyWindowEvent>,
    ) {
        match event {
            WindowEvent::DragEntered { id, .. } => {
                if !event_loop
                    .data_transfer(*id)
                    .is_ok_and(|transfer| transfer.has_type(&TypeHint::UriList))
                {
                    return;
                }

                // We notify Bevy applications of paths, but do not move files from their source.
                if event_loop
                    .set_valid_dnd_actions(*id, &[DndAction::Copy])
                    .is_err()
                {
                    return;
                }

                let state = self.transfers.entry((window, *id)).or_default();
                state.request = event_loop.fetch_data_transfer(*id, &TypeHint::UriList).ok();
            }
            WindowEvent::DragDropped { id, .. } => {
                let key = (window, *id);
                let Some(state) = self.transfers.get_mut(&key) else {
                    return;
                };

                if !state.paths.is_empty() {
                    let state = self.transfers.remove(&key).unwrap();
                    for path_buf in state.paths {
                        events.send(FileDragAndDrop::DroppedFile { window, path_buf });
                    }
                } else {
                    // Some sources only make their data available when the drag has dropped.
                    state.dropped = true;
                    if state.request.is_none() {
                        state.request =
                            event_loop.fetch_data_transfer(*id, &TypeHint::UriList).ok();
                        if state.request.is_none() {
                            self.transfers.remove(&key);
                        }
                    }
                }
            }
            WindowEvent::DragLeft { id } => {
                let key = (window, *id);
                if self.transfers.get(&key).is_some_and(|state| !state.dropped) {
                    let state = self.transfers.remove(&key).unwrap();
                    if !state.paths.is_empty() {
                        events.send(FileDragAndDrop::HoveredFileCanceled { window });
                    }
                }
            }
            WindowEvent::DataTransferReceived { id, serial, value } => {
                // Some Winit backends may send the same result to multiple windows.
                // A fetch serial associates it with the window that requested the data.
                let Some((&key, state)) =
                    self.transfers.iter_mut().find(|((_, transfer_id), state)| {
                        *transfer_id == *id && state.request == Some(*serial)
                    })
                else {
                    return;
                };
                let window = key.0;
                match value.try_as_uris() {
                    Ok(uris) => {
                        state.request = None;
                        let paths = uris.iter().filter_map(|uri| file_path(uri));
                        if state.dropped {
                            for path_buf in paths {
                                events.send(FileDragAndDrop::DroppedFile { window, path_buf });
                            }
                            self.transfers.remove(&key);
                        } else {
                            state.paths.extend(paths);
                            for path_buf in state.paths.iter().cloned() {
                                events.send(FileDragAndDrop::HoveredFile { window, path_buf });
                            }
                        }
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Deadlock
                        ) =>
                    {
                        // Winit may deliver another event for the same fetch when the data is ready.
                    }
                    Err(_) => {
                        state.request = None;
                        if state.dropped {
                            self.transfers.remove(&key);
                        }
                    }
                }
            }
            WindowEvent::Destroyed => self.transfers.retain(|(entity, _), _| *entity != window),
            _ => {}
        }
    }
}

#[cfg(any(unix, windows, target_os = "redox"))]
fn file_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

#[cfg(not(any(unix, windows, target_os = "redox")))]
fn file_path(_uri: &str) -> Option<PathBuf> {
    None
}

#[cfg(all(test, any(unix, windows, target_os = "redox")))]
mod tests {
    use super::*;

    #[test]
    fn file_drag_converts_only_file_uris() {
        let file_uri = if cfg!(windows) {
            "file:///C:/demo/winit%20drop.txt"
        } else {
            "file:///demo/winit%20drop.txt"
        };
        let path = file_path(file_uri).unwrap();
        assert_eq!(path.file_name().unwrap(), "winit drop.txt");
        assert!(file_path("https://example.com/image.png").is_none());
        assert!(file_path("not a URI").is_none());
    }
}
