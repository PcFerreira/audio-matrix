use std::path::PathBuf;

pub fn choose(save: bool, current: &str) -> Result<Option<PathBuf>, String> {
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::Controls::Dialogs::*,
            core::{PCWSTR, PWSTR},
        };
        let mut path = vec![0_u16; 32768];
        let name = std::path::Path::new(current)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("matrix-preset.json");
        for (out, input) in path.iter_mut().zip(name.encode_utf16()) {
            *out = input;
        }
        let filter: Vec<u16> = "Audio Matrix preset (*.json)\0*.json\0All files\0*.*\0\0"
            .encode_utf16()
            .collect();
        let extension: Vec<u16> = "json\0".encode_utf16().collect();
        let title: Vec<u16> = if save {
            "Save Audio Matrix preset as\0"
        } else {
            "Open Audio Matrix preset\0"
        }
        .encode_utf16()
        .collect();
        let initial = std::path::Path::new(current)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .and_then(|p| std::fs::canonicalize(p).ok())
            .map(|p| {
                p.to_string_lossy()
                    .encode_utf16()
                    .chain([0])
                    .collect::<Vec<_>>()
            });
        let mut dialog = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: PWSTR(path.as_mut_ptr()),
            nMaxFile: path.len() as u32,
            lpstrDefExt: PCWSTR(extension.as_ptr()),
            lpstrTitle: PCWSTR(title.as_ptr()),
            lpstrInitialDir: initial
                .as_ref()
                .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            Flags: OFN_EXPLORER
                | OFN_NOCHANGEDIR
                | OFN_PATHMUSTEXIST
                | if save {
                    OFN_OVERWRITEPROMPT
                } else {
                    OFN_FILEMUSTEXIST
                },
            ..Default::default()
        };
        // SAFETY: buffers are NUL-terminated, writable where required, and live
        // for the entire modal call. The picker runs on its own UI helper thread.
        let accepted = unsafe {
            if save {
                GetSaveFileNameW(&mut dialog)
            } else {
                GetOpenFileNameW(&mut dialog)
            }
        }
        .as_bool();
        if accepted {
            let length = path.iter().position(|c| *c == 0).unwrap_or(path.len());
            Ok(Some(PathBuf::from(String::from_utf16_lossy(
                &path[..length],
            ))))
        } else {
            let error = unsafe { CommDlgExtendedError() }.0;
            if error == 0 {
                Ok(None)
            } else {
                Err(format!("File picker failed: {error:#x}"))
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (save, current);
        Err("Native file picker requires Windows".into())
    }
}
