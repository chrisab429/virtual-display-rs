use std::{
    ops::ControlFlow,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
};

use driver_ipc::{DriverCommand, Monitor};
use log::warn;
use wdf_umdf::IddCxMonitorDeparture;
use wdf_umdf_sys::{IDDCX_ADAPTER__, IDDCX_MONITOR__};
use win_pipes::NamedPipeServerOptions;
use winreg::{
    enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    RegKey,
};

use crate::context::DeviceContext;

// Statics are `Mutex<Option<T>>` / `Mutex<Vec<T>>` rather than
// `OnceLock<T>` so the driver tolerates PnP disable+enable cycles
// inside one WUDFHost process. The UMDF host stays alive across
// device toggles, so `OnceLock::set(...).unwrap()` panics on the
// second adapter_init_finished. The listener thread itself is only
// spawned once per process; subsequent inits just clear+repopulate
// monitor state from registry.
pub static ADAPTER: Mutex<Option<AdapterObject>> = Mutex::new(None);
pub static MONITOR_MODES: Mutex<Vec<MonitorObject>> = Mutex::new(Vec::new());
static LISTENER_SPAWNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct AdapterObject(pub NonNull<IDDCX_ADAPTER__>);
unsafe impl Sync for AdapterObject {}
unsafe impl Send for AdapterObject {}

#[derive(Debug)]
pub struct MonitorObject {
    pub monitor_object: Option<NonNull<IDDCX_MONITOR__>>,
    pub monitor: Monitor,
}
unsafe impl Sync for MonitorObject {}
unsafe impl Send for MonitorObject {}

/// WARNING: Locks MONITOR_MODES, don't call if already locked or deadlock happens
pub fn monitor_count() -> usize {
    MONITOR_MODES.lock().unwrap().len()
}

pub fn startup() {
    // PnP re-init in the same process: drop any stale monitor state
    // from the previous adapter so the registry-driven config can
    // repopulate cleanly. Without this the `add()` "id already
    // present" guard rejects every monitor on the second init.
    MONITOR_MODES.lock().unwrap().clear();

    // Registry-driven default monitor config. Runs on every init,
    // not just the first — that's the point: a freshly-written
    // `data` value picked up via disable+enable cycle materializes
    // the new monitor on enable.
    let monitors = get_data();
    if !monitors.is_empty() {
        add(monitors);
    }

    // Pipe listener is per-process, not per-adapter-init. After
    // the first startup() the thread keeps running and the pipe
    // stays bound — subsequent inits share it.
    if LISTENER_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }

    thread::spawn(move || {
        let server = NamedPipeServerOptions::new(r"\\.\pipe\virtualdisplaydriver")
            .reject_remote()
            .read_message()
            .write_message()
            .access_inbound()
            .first_pipe_instance()
            .max_instances(1)
            .in_buffer_size(4096)
            .wait()
            .create()
            .unwrap();

        for client in server.incoming() {
            let Ok(client) = client else {
                // errors are safe to continue on
                continue;
            };

            for data in client.iter_full() {
                let Ok(msg) = std::str::from_utf8(&data) else {
                    _ = server.disconnect();
                    continue;
                };

                let Ok(msg) = serde_json::from_str::<DriverCommand>(msg) else {
                    _ = server.disconnect();
                    continue;
                };

                match msg {
                    DriverCommand::Add(monitors) => add(monitors),

                    DriverCommand::Remove(ids) => {
                        if let Some(ControlFlow::Continue(_)) = remove(ids) {
                            continue;
                        }
                    }

                    DriverCommand::RemoveAll => {
                        if let Some(ControlFlow::Continue(_)) = remove_all() {
                            continue;
                        }
                    }
                }
            }
        }
    });
}

fn get_data() -> Vec<Monitor> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = r"SOFTWARE\VirtualDisplayDriver";

    let Ok(driver_settings) = hklm.open_subkey_with_flags(key, KEY_READ) else {
        return Vec::new();
    };

    driver_settings
        .get_value::<String, _>("data")
        .map(|data| serde_json::from_str::<Vec<Monitor>>(&data).unwrap_or_default())
        .unwrap_or_default()
}

fn add(monitors: Vec<Monitor>) {
    let adapter = {
        let guard = ADAPTER.lock().unwrap();
        let Some(a) = guard.as_ref() else {
            warn!("Cannot add monitors yet; adapter not initialized");
            return;
        };
        a.0.as_ptr()
    };

    unsafe {
        DeviceContext::get_mut(adapter as *mut _, |context| {
            for monitor in monitors {
                let id = monitor.id;

                {
                    let mut lock = MONITOR_MODES.lock().unwrap();

                    // if this monitor index is already in, do not add it, no-op it
                    if lock.iter().any(|m| m.monitor.id == id) {
                        warn!("Cannot add monitor {id}, because it is already added");
                        continue;
                    }

                    lock.push(MonitorObject {
                        monitor_object: None,
                        monitor,
                    });
                }

                context.create_monitor(id);
            }
        })
        .unwrap();
    }
}

fn remove_all() -> Option<ControlFlow<()>> {
    let mut lock = MONITOR_MODES.lock().unwrap();

    for monitor in lock.drain(..) {
        let Some(mut monitor_object) = monitor.monitor_object else {
            return Some(ControlFlow::Continue(()));
        };

        unsafe {
            IddCxMonitorDeparture(monitor_object.as_mut()).unwrap();
        }
    }

    None
}

fn remove(ids: Vec<u32>) -> Option<ControlFlow<()>> {
    let mut lock = MONITOR_MODES.lock().unwrap();

    let mut to_remove = Vec::new();

    for &id in ids.iter() {
        for (i, monitor) in lock.iter().enumerate() {
            if id == monitor.monitor.id {
                to_remove.push(i);

                let Some(mut monitor_object) = monitor.monitor_object else {
                    return Some(ControlFlow::Continue(()));
                };

                unsafe {
                    IddCxMonitorDeparture(monitor_object.as_mut()).unwrap();
                }
            }
        }
    }

    for r_id in to_remove {
        lock.remove(r_id);
    }

    None
}
