use fsw_core::navigation::NavigationAction;
use std::{error::Error, path::PathBuf};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn files_including_executables_and_shortcuts_are_only_selected() -> Result<(), Box<dyn Error>> {
    let root = Scratch(std::env::temp_dir().join(format!("fsw-navigation-{}", std::process::id())));
    std::fs::create_dir(&root.0)?;
    assert!(matches!(
        NavigationAction::for_path(&root.0.to_string_lossy())?,
        NavigationAction::OpenDirectory(_)
    ));
    for name in [
        "payload.exe",
        "payload.cmd",
        "payload.ps1",
        "payload.lnk",
        "slash.html",
        "space # percent %.txt",
    ] {
        let path = root.0.join(name);
        std::fs::write(&path, b"test")?;
        assert_eq!(
            NavigationAction::for_path(&path.to_string_lossy())?,
            NavigationAction::SelectFile(path.to_string_lossy().into_owned())
        );
    }
    #[cfg(windows)]
    {
        let action = NavigationAction::for_path(&root.0.to_string_lossy())?;
        assert!(
            action
                .navigate_if(|| false)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::Interrupted)
        );
    }
    Ok(())
}

#[test]
fn relative_and_shell_namespace_inputs_are_rejected() {
    for path in [
        "payload.exe",
        "shell:AppsFolder",
        "::{00000000-0000-0000-0000-000000000000}",
        "bad\0path",
    ] {
        assert!(NavigationAction::for_path(path).is_err());
    }
}

#[cfg(windows)]
#[test]
fn symlinks_are_classified_by_their_targets() -> Result<(), Box<dyn Error>> {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let root =
        Scratch(std::env::temp_dir().join(format!("fsw-navigation-links-{}", std::process::id())));
    std::fs::create_dir(&root.0)?;
    let directory = root.0.join("directory");
    let file = root.0.join("payload.exe");
    let directory_link = root.0.join("directory-link");
    let file_link = root.0.join("payload-link.exe");
    std::fs::create_dir(&directory)?;
    std::fs::write(&file, b"test")?;
    if let Err(error) = symlink_dir(&directory, &directory_link) {
        // Developer Mode may be disabled and the test token may not have
        // SeCreateSymbolicLinkPrivilege. The production behavior remains
        // covered on systems where Windows permits test link creation.
        if error.raw_os_error() == Some(1314) {
            eprintln!("skipping symlink classification: link creation is not permitted");
            return Ok(());
        }
        return Err(error.into());
    }
    symlink_file(&file, &file_link)?;

    assert_eq!(
        NavigationAction::for_path(&directory_link.to_string_lossy())?,
        NavigationAction::OpenDirectory(directory_link.to_string_lossy().into_owned())
    );
    assert_eq!(
        NavigationAction::for_path(&file_link.to_string_lossy())?,
        NavigationAction::SelectFile(file_link.to_string_lossy().into_owned())
    );
    Ok(())
}
