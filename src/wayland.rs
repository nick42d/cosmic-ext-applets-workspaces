use calloop::channel::*;
use cctk::{
    sctk::{
        self,
        output::{OutputHandler, OutputState},
        reexports::{
            calloop,
            calloop_wayland_source::WaylandSource,
            client::{self as wayland_client},
            protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1,
        },
        registry::{ProvidesRegistryState, RegistryState},
    },
    toplevel_info::{ToplevelInfo, ToplevelInfoHandler, ToplevelInfoState},
    workspace::{Workspace, WorkspaceHandler, WorkspaceState},
};
use futures::{SinkExt, channel::mpsc, executor::block_on};
use std::os::{
    fd::{FromRawFd, RawFd},
    unix::net::UnixStream,
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::wl_output::{self, WlOutput},
};

#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    Activate(ExtWorkspaceHandleV1),
}

pub fn spawn_workspaces(
    tx: mpsc::Sender<(Vec<Workspace>, Vec<ToplevelInfo>)>,
) -> SyncSender<WorkspaceEvent> {
    let (workspaces_tx, workspaces_rx) = calloop::channel::sync_channel(100);

    let socket = std::env::var("X_PRIVILEGED_WAYLAND_SOCKET")
        .ok()
        .and_then(|fd| {
            fd.parse::<RawFd>()
                .ok()
                .map(|fd| unsafe { UnixStream::from_raw_fd(fd) })
        });

    let conn = if let Some(socket) = socket {
        Connection::from_socket(socket)
    } else {
        Connection::connect_to_env()
    }
    .map_err(anyhow::Error::msg);

    if let Ok(conn) = conn {
        std::thread::spawn(move || {
            let configured_output = std::env::var("COSMIC_PANEL_OUTPUT").ok();
            let mut event_loop = calloop::EventLoop::<State>::try_new().unwrap();
            let loop_handle = event_loop.handle();
            let (globals, event_queue) = registry_queue_init(&conn).unwrap();
            let qhandle = event_queue.handle();

            WaylandSource::new(conn, event_queue)
                .insert(loop_handle)
                .unwrap();

            let registry_state = RegistryState::new(&globals);
            let mut state = State {
                // Must be before `WorkspaceState`
                output_state: OutputState::new(&globals, &qhandle),
                configured_output,
                workspace_state: WorkspaceState::new(&registry_state, &qhandle),
                toplevel_info_state: ToplevelInfoState::new(&registry_state, &qhandle),
                registry_state,
                expected_output: None,
                tx,
                running: true,
                have_workspaces: false,
            };
            let loop_handle = event_loop.handle();
            loop_handle
                .insert_source(workspaces_rx, |e, (), state| match e {
                    Event::Msg(WorkspaceEvent::Activate(handle)) => {
                        handle.activate();
                        state
                            .workspace_state
                            .workspace_manager()
                            .get()
                            .unwrap()
                            .commit();
                    }
                    Event::Closed => {
                        if let Ok(workspace_manager) =
                            state.workspace_state.workspace_manager().get()
                        {
                            for g in state.workspace_state.workspace_groups() {
                                g.handle.destroy();
                            }
                            workspace_manager.stop();
                        }
                    }
                })
                .unwrap();
            while state.running {
                event_loop.dispatch(None, &mut state).unwrap();
            }
        });
    } else {
        eprintln!("ENV variable WAYLAND_DISPLAY is missing. Exiting...");
        std::process::exit(1);
    }

    workspaces_tx
}

#[derive(Debug)]
pub struct State {
    running: bool,
    tx: mpsc::Sender<(Vec<Workspace>, Vec<cctk::toplevel_info::ToplevelInfo>)>,
    configured_output: Option<String>,
    expected_output: Option<WlOutput>,
    output_state: OutputState,
    registry_state: RegistryState,
    workspace_state: WorkspaceState,
    toplevel_info_state: ToplevelInfoState,
    have_workspaces: bool,
}

impl State {
    pub fn workspace_list(&self) -> Vec<Workspace> {
        self.workspace_state
            .workspace_groups()
            .filter(|g| {
                g.outputs
                    .iter()
                    .any(|o| Some(o) == self.expected_output.as_ref())
            })
            .flat_map(|g| {
                g.workspaces
                    .iter()
                    .filter_map(|handle| self.workspace_state.workspace_info(handle))
            })
            .cloned()
            .collect()
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    sctk::registry_handlers![OutputState,];
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let info = self.output_state.info(&output).unwrap();
        let is_target = match &self.configured_output {
            Some(target) => info.name.as_deref() == Some(target),
            None => self.expected_output.is_none(),
        };

        if is_target {
            self.expected_output = Some(output);
            if self.have_workspaces {
                let _ = block_on(self.tx.send((
                    self.workspace_list(),
                    self.toplevel_info_state.toplevels().cloned().collect(),
                )));
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WorkspaceHandler for State {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }

    fn done(&mut self) {
        self.have_workspaces = true;
        let _ = block_on(self.tx.send((
            self.workspace_list(),
            self.toplevel_info_state.toplevels().cloned().collect(),
        )));
    }
}

impl ToplevelInfoHandler for State {
    fn toplevel_info_state(&mut self) -> &mut cctk::toplevel_info::ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    fn new_toplevel(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        toplevel: &cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        let _ = block_on(self.tx.send((
            self.workspace_list(),
            self.toplevel_info_state.toplevels().cloned().collect(),
        )));
    }

    fn update_toplevel(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        toplevel: &cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        let _ = block_on(self.tx.send((
            self.workspace_list(),
            self.toplevel_info_state.toplevels().cloned().collect(),
        )));
    }

    fn toplevel_closed(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        toplevel: &cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        let _ = block_on(self.tx.send((
            self.workspace_list(),
            self.toplevel_info_state.toplevels().cloned().collect(),
        )));
    }
    fn info_done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>) {
        let _ = block_on(self.tx.send((
            self.workspace_list(),
            self.toplevel_info_state.toplevels().cloned().collect(),
        )));
    }
}

cctk::delegate_toplevel_info!(State);
cctk::delegate_workspace!(State);
sctk::delegate_output!(State);
sctk::delegate_registry!(State);
