//! Launch only the update components shipped beside this application's executable.

use std::path::{Path, PathBuf};

fn packaged_child(parent: &Path, name: &str, directory: bool) -> Result<PathBuf, String> {
    let child = parent.join(name);
    let metadata = std::fs::symlink_metadata(&child)
        .map_err(|_| "更新组件不完整，请完整解压程序包。".to_owned())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("更新组件不能使用符号链接或目录联接。".to_owned());
        }
    }
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err("更新组件不是随程序发布的普通文件。".to_owned());
    }
    let canonical = child.canonicalize().map_err(|error| error.to_string())?;
    let expected_parent = parent.canonicalize().map_err(|error| error.to_string())?;
    if canonical.parent() != Some(expected_parent.as_path()) {
        return Err("更新组件不在程序目录中。".to_owned());
    }
    // Keep the original absolute Windows path for Windows PowerShell 5.1.
    Ok(child)
}

#[cfg(windows)]
fn system_powershell() -> Result<PathBuf, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err("无法定位 Windows PowerShell。".to_owned());
    }
    use std::os::windows::ffi::OsStringExt;
    let directory = std::ffi::OsString::from_wide(&buffer[..length as usize]);
    let executable = PathBuf::from(directory)
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !executable.is_file() {
        return Err("Windows PowerShell 不可用。".to_owned());
    }
    Ok(executable)
}

#[cfg(windows)]
pub fn open_update_window() -> Result<(), String> {
    launch_updates(false)
}

#[cfg(windows)]
pub fn launch_updates(startup: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let base = executable.parent().ok_or("无法定位程序目录。")?;
    let resources = packaged_child(base, "updates", true)?;
    let script = packaged_child(&resources, "AppUpdates.ps1", false)?;
    let config = packaged_child(&resources, "app-update.json", false)?;
    let mut command = Command::new(system_powershell()?);
    command
        .args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .arg("-Config")
        .arg(config);
    if startup {
        command.arg("-StartupCheck");
    }
    command.current_dir(base)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW; the script owns its GUI.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开更新窗口：{error}"))
}

#[cfg(not(windows))]
pub fn open_update_window() -> Result<(), String> {
    Err("此更新入口仅适用于 Windows 版本。".to_owned())
}

#[cfg(test)]
mod tests {
    use super::packaged_child;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn packaged_resources_are_fixed_children_and_missing_files_fail() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowse-update-path-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let module = root.join("AppUpdates.ps1");
        std::fs::write(&module, "# test component").unwrap();
        assert_eq!(
            packaged_child(&root, "AppUpdates.ps1", false).unwrap(),
            module
        );
        assert!(packaged_child(&root, "missing.ps1", false).is_err());
        assert!(packaged_child(&root, "AppUpdates.ps1", true).is_err());
        assert!(packaged_child(&root, "..", true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
