use log::debug;
use shared::args::{ExecMode, OnFail};
use shared::exec::exec;
use shared::returncode_eval::exec_eval;

/// Enable one or more systemd units inside the target system with a single
/// `systemctl` invocation.
pub fn enable_services<'a>(units: impl IntoIterator<Item = &'a str>) {
    let units: Vec<&str> = units.into_iter().collect();
    if units.is_empty() {
        return;
    }
    debug!("Enabling {units:?}");
    exec_eval(
        exec(
            ExecMode::Chroot { root: "/mnt" },
            "systemctl",
            ["enable"].into_iter().chain(units.iter().copied()),
            OnFail::Error,
        ),
        &format!("Enable {}", units.join(", ")),
    );
}

pub fn enable_service(unit: &str) {
    enable_services([unit]);
}
