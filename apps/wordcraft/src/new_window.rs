//! Fork patch (multi-window): start another WordCraft window as a new process.
//!
//! macOS: inside an app bundle, `open -n` starts a second instance of the bundle (LaunchServices
//! would otherwise just bring the running one to the front). Elsewhere, or when run from a build
//! folder, the executable is started directly. `--new-window` tells the new instance to open a
//! little below and to the right of the saved window position, so it doesn't hide this one.

use std::process::Command;

pub const FLAG: &str = "--new-window";

pub fn launch(path: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    let bundle = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(std::path::Path::to_path_buf);
    #[cfg(not(target_os = "macos"))]
    let bundle: Option<std::path::PathBuf> = None;
    let mut cmd = match bundle {
        Some(app) => {
            let mut c = Command::new("/usr/bin/open");
            c.arg("-n").arg(app).arg("--args").arg(FLAG);
            c
        }
        None => {
            let mut c = Command::new(exe);
            c.arg(FLAG);
            c
        }
    };
    if let Some(p) = path {
        cmd.arg(p);
    }
    // The new window must not take over this one's control channel.
    cmd.env_remove("WORDCRAFT_CONTROL_PORT");
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // Reap it when it ends, so no zombie process is left behind.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    log::info!("new window{}", path.map(|p| format!(": {p}")).unwrap_or_default());
    Ok(())
}
